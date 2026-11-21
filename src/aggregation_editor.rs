//! Draft-only aggregation controls and a bounded, virtualized result view.
//! The parent owns snapshots, cancellation and execution; this Entity only
//! retains the current Arrow aggregate result and formats its visible cells.
use crate::controls::{self, ButtonStyle};
use std::{ops::Range, sync::Arc};

use anyhow::{Result, ensure};
use gpui::{
    App, Context, Corner, ElementId, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, Render, ScrollHandle, Subscription,
    UniformListScrollHandle, Window, anchored, deferred, div, point, prelude::*, px, rgb,
    uniform_list,
};

use crate::{
    aggregation::{
        AggregateBucket, AggregateMetric, AggregateOp, AggregateRequest, AggregateResult,
        supported_ops, supports_bucket, supports_group,
    },
    aggregation_charts::AggregateCharts,
    engine::ColumnInfo,
    i18n::t,
    icons::{self, Icon},
    text_input::TextInput,
    theme::Colors,
};

const CELL_WIDTH: f32 = 140.;
const CELL_HEIGHT: f32 = 26.;
const DISPLAY_CHARS: usize = 120;
const PICKER_MAX_WIDTH: f32 = 316.;
const PICKER_ROW_HEIGHT: f32 = 30.;
const PICKER_VISIBLE_ROWS: usize = 6;
const BUCKETS: [Option<AggregateBucket>; 6] = [
    None,
    Some(AggregateBucket::YearPart),
    Some(AggregateBucket::QuarterPart),
    Some(AggregateBucket::MonthPart),
    Some(AggregateBucket::WeekPart),
    Some(AggregateBucket::DayPart),
];

pub enum AggregationEvent {
    Run,
    Cancel,
    Close,
    Export,
    DrillDown(usize),
    RestoreDetails,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Groups,
    GroupField(usize),
    Field(usize),
    Operator(usize),
    Bucket(usize),
}

impl Picker {
    fn is_field(self) -> bool {
        matches!(self, Self::Groups | Self::GroupField(_) | Self::Field(_))
    }

    fn multiple(self) -> bool {
        matches!(self, Self::Groups | Self::Operator(_) | Self::Bucket(_))
    }

    fn label(self) -> &'static str {
        match self {
            Self::Groups => "Choose group fields",
            Self::GroupField(_) => "Group by",
            Self::Field(_) => "Choose summary field",
            Self::Operator(_) => "Choose aggregation",
            Self::Bucket(_) => "Time grouping",
        }
    }
}

#[derive(Clone, Copy)]
enum PickerOption {
    Field(Option<usize>),
    Operator(AggregateOp),
    Bucket(Option<AggregateBucket>),
}

struct CachedField {
    column: ColumnInfo,
    search: String,
    ops: Vec<AggregateOp>,
}

struct MeasureRow {
    id: usize,
    source: MeasureSource,
    ops: Vec<AggregateOp>,
}

impl MeasureRow {
    fn new(id: usize) -> Self {
        Self {
            id,
            source: MeasureSource::Unselected,
            ops: Vec::new(),
        }
    }

    fn append_metrics(&self, metrics: &mut Vec<AggregateMetric>) -> Result<()> {
        ensure!(
            self.source != MeasureSource::Unselected,
            "{}",
            t("Choose a summary field or select All records.")
        );
        ensure!(
            !self.ops.is_empty(),
            "{}",
            t("Choose at least one aggregation.")
        );
        metrics.extend(self.ops.iter().map(|op| AggregateMetric {
            field: self.source.column().map(str::to_owned),
            op: *op,
        }));
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
enum MeasureSource {
    Unselected,
    AllRecords,
    Column(String),
}

impl MeasureSource {
    fn column(&self) -> Option<&str> {
        match self {
            Self::Column(name) => Some(name),
            Self::Unselected | Self::AllRecords => None,
        }
    }

    fn supported_ops<'a>(&self, fields: &'a [CachedField]) -> &'a [AggregateOp] {
        match self {
            Self::Unselected => &[],
            Self::AllRecords => &[AggregateOp::CountRows],
            Self::Column(name) => fields
                .iter()
                .find(|field| field.column.name == *name)
                .map_or(&[], |field| field.ops.as_slice()),
        }
    }
}

struct GroupRow {
    id: usize,
    field: String,
    // An empty selection requires a choice for temporal fields. A single
    // None represents the explicit Exact value option.
    buckets: Vec<Option<AggregateBucket>>,
}

impl GroupRow {
    fn toggle_bucket(&mut self, bucket: Option<AggregateBucket>) {
        if let Some(index) = self.buckets.iter().position(|selected| *selected == bucket) {
            self.buckets.remove(index);
            return;
        }
        if bucket.is_none() || self.buckets.contains(&None) {
            self.buckets.clear();
        }
        self.buckets.push(bucket);
    }

    fn append_to_request(&self, temporal: bool, request: &mut AggregateRequest) -> Result<()> {
        ensure!(
            !temporal || !self.buckets.is_empty(),
            "{}",
            t("Choose at least one time grouping for {field}.").replace("{field}", &self.field)
        );
        request.group_by.push(self.field.clone());
        request.group_buckets.extend(
            self.buckets
                .iter()
                .flatten()
                .map(|bucket| (self.field.clone(), *bucket)),
        );
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowSection {
    Group,
    Measure,
}

struct RowDrag {
    editor: EntityId,
    section: RowSection,
    id: usize,
    name: String,
    dark: bool,
}

struct RowDragPreview {
    name: String,
    dark: bool,
}

impl Render for RowDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let c = Colors::new(self.dark);
        div()
            .mt(px(18.))
            .ml(px(18.))
            .max_w(px(228.))
            .px_2()
            .py_1()
            .truncate()
            .rounded(px(4.))
            .shadow_md()
            .border_1()
            .border_color(rgb(c.accent))
            .bg(rgb(c.surface))
            .text_size(px(12.))
            .text_color(rgb(c.text))
            .child(self.name.clone())
    }
}

pub struct AggregationEditor {
    columns: Vec<ColumnInfo>,
    fields: Vec<CachedField>,
    groups: Vec<GroupRow>,
    measures: Vec<MeasureRow>,
    next_id: usize,
    dark: bool,
    busy: bool,
    dirty: bool,
    source_changed: bool,
    error: Option<String>,
    result: Option<Arc<AggregateResult>>,
    chart_metrics: Arc<[AggregateMetric]>,
    pub(crate) charts: Option<AggregateCharts>,
    result_revision: u64,
    result_group_count: usize,
    detail_group: Option<usize>,
    detail_open: bool,
    drilling_down: bool,
    picker: Option<Picker>,
    // Recomputed only for a schema/search/picker change, never per visible row.
    matching_fields: Vec<Option<usize>>,
    search: Entity<TextInput>,
    focus: FocusHandle,
    host_focus: Option<FocusHandle>,
    results_expanded: bool,
    charts_expanded: bool,
    picker_scroll: UniformListScrollHandle,
    result_scroll: UniformListScrollHandle,
    horizontal: ScrollHandle,
    viewport_width: f32,
    _search_subscription: Subscription,
}

impl EventEmitter<AggregationEvent> for AggregationEditor {}

impl AggregationEditor {
    pub fn new(columns: Vec<ColumnInfo>, dark: bool, cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| {
            if let Some(result) = this.result.take() {
                cx.background_executor()
                    .spawn(async move { drop(result) })
                    .detach();
            }
        })
        .detach();
        let search = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(dark)
                .with_placeholder(&t("Search fields"))
        });
        let subscription = cx.subscribe(&search, |this, _, _, cx| {
            this.refresh_matches(cx);
            this.picker_scroll = UniformListScrollHandle::new();
            cx.notify();
        });
        let mut editor = Self {
            fields: Self::cache_fields(&columns),
            columns,
            groups: Vec::new(),
            measures: Vec::new(),
            next_id: 0,
            dark,
            busy: false,
            dirty: false,
            source_changed: false,
            error: None,
            result: None,
            chart_metrics: Arc::default(),
            charts: None,
            result_revision: 0,
            result_group_count: 0,
            detail_group: None,
            detail_open: false,
            drilling_down: false,
            picker: None,
            matching_fields: Vec::new(),
            search,
            focus: cx.focus_handle(),
            host_focus: None,
            results_expanded: false,
            charts_expanded: false,
            picker_scroll: UniformListScrollHandle::new(),
            result_scroll: UniformListScrollHandle::new(),
            horizontal: ScrollHandle::new(),
            viewport_width: 336.,
            _search_subscription: subscription,
        };
        editor.push_measure();
        editor
    }

    fn cache_fields(columns: &[ColumnInfo]) -> Vec<CachedField> {
        columns
            .iter()
            .filter(|column| supports_group(&column.data_type))
            .map(|column| CachedField {
                search: format!("{} {}", column.name, column.data_type).to_lowercase(),
                ops: supported_ops(&column.data_type),
                column: column.clone(),
            })
            .collect()
    }

    pub fn request(&self) -> Result<AggregateRequest> {
        ensure!(
            !self.measures.is_empty(),
            "{}",
            t("Add at least one summary measure.")
        );
        let mut request = AggregateRequest::default();
        for row in &self.groups {
            let temporal = self.fields.iter().any(|field| {
                field.column.name == row.field && supports_bucket(&field.column.data_type)
            });
            row.append_to_request(temporal, &mut request)?;
        }
        for row in &self.measures {
            row.append_metrics(&mut request.metrics)?;
        }
        Ok(request)
    }

    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        self.drilling_down = false;
        if busy {
            self.picker = None;
            self.error = None;
        }
        cx.notify();
    }

    pub fn set_drilldown_busy(&mut self, cx: &mut Context<Self>) {
        self.set_busy(true, cx);
        self.drilling_down = true;
    }

    pub fn set_detail_view(&mut self, open: bool, row: Option<usize>, cx: &mut Context<Self>) {
        self.detail_open = open;
        self.detail_group = row;
        cx.notify();
    }

    pub fn result_for_export(&self) -> Option<Arc<AggregateResult>> {
        self.result
            .as_ref()
            .filter(|_| !self.busy && !self.dirty)
            .cloned()
    }

    pub fn drilldown_input(&self, row: usize) -> Option<(Arc<AggregateResult>, usize)> {
        self.result_for_export()
            .filter(|result| self.result_group_count > 0 && row < result.data.row_count())
            .map(|result| (result, self.result_group_count))
    }

    fn replace_result(&mut self, result: Option<Arc<AggregateResult>>, cx: &App) {
        if let Some(previous) = std::mem::replace(&mut self.result, result) {
            cx.background_executor()
                .spawn(async move { drop(previous) })
                .detach();
        }
        self.results_expanded = self.result.is_some();
        self.charts_expanded = self.result.is_some();
        self.result_scroll = UniformListScrollHandle::new();
        self.horizontal = ScrollHandle::new();
        self.result_revision = self.result_revision.wrapping_add(1);
        self.chart_metrics = Arc::default();
        self.charts = None;
        self.detail_group = None;
    }

    pub fn set_result(
        &mut self,
        result: Arc<AggregateResult>,
        metrics: Arc<[AggregateMetric]>,
        charts: Option<AggregateCharts>,
        cx: &mut Context<Self>,
    ) {
        self.result_group_count = result.group_count();
        self.replace_result(Some(result), cx);
        self.chart_metrics = metrics;
        self.charts = charts;
        self.busy = false;
        self.dirty = false;
        self.source_changed = false;
        self.error = None;
        cx.notify();
    }

    pub(crate) fn chart_input(
        &self,
    ) -> Option<(u64, Arc<AggregateResult>, Arc<[AggregateMetric]>)> {
        Some((
            self.result_revision,
            self.result.as_ref()?.clone(),
            self.chart_metrics.clone(),
        ))
    }

    pub(crate) fn set_charts(
        &mut self,
        revision: u64,
        charts: Option<AggregateCharts>,
        cx: &mut Context<Self>,
    ) {
        if self.result_revision == revision {
            self.charts = charts;
            cx.notify();
        }
    }

    pub fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.busy = false;
        self.drilling_down = false;
        self.error = Some(error);
        cx.notify();
    }

    /// Call for every changed source snapshot, even when its schema is equal.
    pub fn clear_result(&mut self, cx: &mut Context<Self>) {
        self.source_changed |= self.result.is_some();
        self.replace_result(None, cx);
        self.result_group_count = 0;
        self.drilling_down = false;
        self.dirty = false;
        self.error = None;
        self.busy = false;
        self.picker = None;
        cx.notify();
    }

    /// Preserve compatible drafts; the parent separately invalidates results
    /// whenever the underlying immutable snapshot changes.
    pub fn reconcile_columns(&mut self, columns: Vec<ColumnInfo>, cx: &mut Context<Self>) {
        if self.columns == columns {
            return;
        }
        self.fields = Self::cache_fields(&columns);
        self.columns = columns;
        self.groups.retain_mut(|row| {
            let Some(field) = self
                .fields
                .iter()
                .find(|field| field.column.name == row.field)
            else {
                return false;
            };
            if !supports_bucket(&field.column.data_type) {
                row.buckets.clear();
            }
            true
        });
        for row in &mut self.measures {
            let ops = row.source.supported_ops(&self.fields);
            row.ops.retain(|op| ops.contains(op));
            if ops.is_empty() {
                row.source = MeasureSource::Unselected;
            }
        }
        self.picker = None;
        self.refresh_matches(cx);
        self.clear_result(cx);
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        self.search.update(cx, |input, cx| input.set_dark(dark, cx));
        cx.notify();
    }

    pub fn refresh_language(&mut self, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| {
            input.set_placeholder(&t("Search fields"), cx)
        });
        self.refresh_matches(cx);
        cx.notify();
    }

    pub fn set_host_focus(&mut self, focus: FocusHandle) {
        self.host_focus = Some(focus);
    }

    pub(crate) fn dismiss_picker(&mut self, cx: &mut Context<Self>) {
        self.picker = None;
        cx.notify();
    }

    pub fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.picker.is_some_and(Picker::is_field) && self.search.focus_handle(cx).is_focused(window)
    }

    fn push_measure(&mut self) {
        self.measures.push(MeasureRow::new(self.next_id));
        self.next_id += 1;
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.dirty = self.result.is_some();
        self.error = None;
        cx.notify();
    }

    fn refresh_matches(&mut self, cx: &App) {
        self.matching_fields.clear();
        if !self.picker.is_some_and(Picker::is_field) {
            return;
        }
        let search = self.search.read(cx).text().trim().to_lowercase();
        if matches!(self.picker, Some(Picker::Field(_)))
            && (search.is_empty() || t("All records").to_lowercase().contains(&search))
        {
            self.matching_fields.push(None);
        }
        self.matching_fields
            .extend(self.fields.iter().enumerate().filter_map(|(index, field)| {
                if matches!(self.picker, Some(Picker::GroupField(id)) if self.groups.iter().any(|row| row.id != id && row.field == field.column.name)) {
                    return None;
                }
                (search.is_empty() || field.search.contains(&search)).then_some(Some(index))
            }));
    }

    fn open_picker(&mut self, picker: Picker, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.picker == Some(picker) {
            self.close_picker(window, cx);
            return;
        }
        self.picker = Some(picker);
        self.picker_scroll = UniformListScrollHandle::new();
        if picker.is_field() {
            self.search.update(cx, |input, cx| input.set_text("", cx));
            self.refresh_matches(cx);
            self.search.focus_handle(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
        cx.notify();
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        self.focus.focus(window);
        cx.notify();
    }

    fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.close_picker(window, cx);
        } else if self.busy {
            cx.emit(AggregationEvent::Cancel);
        } else {
            if let Some(host) = &self.host_focus {
                host.focus(window);
            }
            cx.emit(AggregationEvent::Close);
        }
    }

    fn ops_for(&self, id: usize) -> &[AggregateOp] {
        self.measures
            .iter()
            .find(|row| row.id == id)
            .map_or(&[], |row| row.source.supported_ops(&self.fields))
    }

    fn select_field(
        &mut self,
        picker: Picker,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A source/schema update may invalidate an option from the previous
        // painted frame before its click is dispatched.
        if self.busy || self.picker != Some(picker) {
            return;
        }
        if index.is_some_and(|index| index >= self.fields.len()) {
            self.close_picker(window, cx);
            return;
        }
        let name = index
            .and_then(|index| self.fields.get(index))
            .map(|field| field.column.name.clone());
        match picker {
            Picker::Groups => {
                if let Some(name) = name {
                    if let Some(index) = self.groups.iter().position(|row| row.field == name) {
                        self.groups.remove(index);
                    } else {
                        self.groups.push(GroupRow {
                            id: self.next_id,
                            field: name,
                            buckets: Vec::new(),
                        });
                        self.next_id += 1;
                    }
                    self.search.focus_handle(cx).focus(window);
                }
            }
            Picker::GroupField(id) => {
                if let Some(name) = name
                    && !self
                        .groups
                        .iter()
                        .any(|row| row.id != id && row.field == name)
                    && let Some(row) = self.groups.iter_mut().find(|row| row.id == id)
                    && row.field != name
                {
                    row.field = name;
                    row.buckets.clear();
                }
                self.close_picker(window, cx);
            }
            Picker::Field(id) => {
                if let Some(row) = self.measures.iter_mut().find(|row| row.id == id) {
                    row.source = name.map_or(MeasureSource::AllRecords, MeasureSource::Column);
                    let ops = row.source.supported_ops(&self.fields);
                    row.ops.retain(|op| ops.contains(op));
                }
                self.close_picker(window, cx);
            }
            Picker::Operator(_) | Picker::Bucket(_) => {}
        }
        self.changed(cx);
    }

    fn button(
        &self,
        id: impl Into<ElementId>,
        label: &str,
        enabled: bool,
        primary: bool,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.dark);
        let style = if primary {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Standard
        };
        controls::button(id, c, style, enabled)
            .child(t(label))
            .when(enabled, |el| {
                el.on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
            })
    }

    fn selector(
        &self,
        id: impl Into<ElementId>,
        picker: Picker,
        hint: String,
        value: impl IntoElement,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.dark);
        let open = self.picker == Some(picker);
        let enabled = !self.busy
            && match picker {
                Picker::Groups => !self.fields.is_empty(),
                Picker::Operator(id) => !self.ops_for(id).is_empty(),
                _ => true,
            };
        controls::selector(id, c, open, enabled)
            .relative()
            .child(value)
            .child(controls::chevron(c, open))
            .when(!open, |el| el.tooltip(icons::tooltip(hint, self.dark)))
            .when(enabled, |el| {
                el.on_click(
                    cx.listener(move |this, _, window, cx| this.open_picker(picker, window, cx)),
                )
            })
            .when(open, |el| el.child(self.picker_anchor(picker, cx)))
    }

    fn field_selector(
        &self,
        id: impl Into<ElementId>,
        picker: Picker,
        value: Option<&str>,
        detail: &str,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.dark);
        let name = value.map_or_else(|| t("Select field"), str::to_owned);
        let hint = if detail.is_empty() {
            name.clone()
        } else {
            format!("{name} · {detail}")
        };
        self.selector(
            id,
            picker,
            hint,
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(value.is_some(), |el| {
                    el.font_weight(gpui::FontWeight::MEDIUM)
                })
                .when(value.is_none(), |el| el.text_color(rgb(c.muted)))
                .child(name),
            cx,
        )
    }

    fn selected_options(
        &self,
        id: impl Into<ElementId>,
        picker: Picker,
        selections: impl IntoIterator<Item = (String, String)>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.dark);
        let selections = selections.into_iter().collect::<Vec<_>>();
        let hint = selections
            .iter()
            .map(|(_, full)| full.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        self.selector(
            id,
            picker,
            hint,
            div()
                .id("aggregate-selected-options")
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .items_center()
                .gap(px(3.))
                .overflow_x_scroll()
                .children(selections.into_iter().map(|(short, _)| {
                    div()
                        .h(px(18.))
                        .px_1()
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .rounded(px(3.))
                        .bg(rgb(c.accent_soft))
                        .text_size(px(10.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(c.accent))
                        .child(short)
                })),
            cx,
        )
    }

    fn ordered_row(
        &self,
        section: RowSection,
        id: usize,
        name: &str,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.dark);
        let editor = cx.entity_id();
        div()
            .id(("aggregate-ordered-row", id))
            .min_w_0()
            .h(px(34.))
            .flex()
            .items_center()
            .gap_1()
            .pr(px(3.))
            .rounded(px(5.))
            .border_1()
            .border_color(rgb(c.grid))
            .bg(rgb(c.surface))
            .when(!self.busy, |el| {
                el.drag_over::<RowDrag>(move |style, drag, _, _| {
                    if drag.editor == editor && drag.section == section && drag.id != id {
                        style.bg(rgb(c.accent_soft)).border_color(rgb(c.accent))
                    } else {
                        style
                    }
                })
                .can_drop(move |drag, _, _| {
                    drag.downcast_ref::<RowDrag>().is_some_and(|drag| {
                        drag.editor == editor && drag.section == section && drag.id != id
                    })
                })
                .on_drop(cx.listener(move |this, drag: &RowDrag, _, cx| {
                    if this.busy {
                        return;
                    }
                    let reordered = match section {
                        RowSection::Group => move_row(&mut this.groups, drag.id, id, |row| row.id),
                        RowSection::Measure => {
                            move_row(&mut this.measures, drag.id, id, |row| row.id)
                        }
                    };
                    if reordered {
                        this.dismiss_picker(cx);
                        this.changed(cx);
                    }
                }))
            })
            .child(
                div()
                    .id(("aggregate-reorder-handle", id))
                    .w(px(18.))
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .justify_center()
                    .items_center()
                    .text_color(rgb(c.muted))
                    .child("⠿")
                    .when(!self.busy, |el| {
                        el.cursor_move()
                            .hover(|el| el.text_color(rgb(c.accent)))
                            .tooltip(icons::tooltip(t("Drag to reorder"), self.dark))
                            .on_drag(
                                RowDrag {
                                    editor,
                                    section,
                                    id,
                                    name: name.to_owned(),
                                    dark: self.dark,
                                },
                                |drag, _, _, cx| {
                                    cx.new(|_| RowDragPreview {
                                        name: drag.name.clone(),
                                        dark: drag.dark,
                                    })
                                },
                            )
                    }),
            )
    }

    fn render_measure(&self, row: &MeasureRow, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let id = row.id;
        let all_records = t("All records");
        let name = match &row.source {
            MeasureSource::Unselected => None,
            MeasureSource::AllRecords => Some(all_records.as_str()),
            MeasureSource::Column(name) => Some(name.as_str()),
        };
        let field = row
            .source
            .column()
            .and_then(|name| self.fields.iter().find(|field| field.column.name == name));
        let detail = field.map_or("", |field| field.column.data_type.as_str());
        let c = Colors::new(self.dark);
        let methods = if row.ops.is_empty() {
            self.selector(
                ("aggregate-op", id),
                Picker::Operator(id),
                t("Choose aggregation"),
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(c.muted))
                    .child(t("Choose aggregation")),
                cx,
            )
        } else {
            self.selected_options(
                ("aggregate-op", id),
                Picker::Operator(id),
                row.ops.iter().map(|op| {
                    let full = t(op.label());
                    let short = if op.label().contains('(') {
                        full.clone()
                    } else {
                        full.split('（').next().unwrap_or(&full).to_owned()
                    };
                    (short, full)
                }),
                cx,
            )
        };
        self.ordered_row(
            RowSection::Measure,
            id,
            name.unwrap_or(&t("Summary field")),
            cx,
        )
        .child(
            self.field_selector(("aggregate-field", id), Picker::Field(id), name, detail, cx)
                .w(gpui::relative(0.32))
                .flex_shrink_0(),
        )
        .child(methods.flex_1())
        .child(self.remove_button(
            ("aggregate-remove", id),
            move |this, cx| {
                this.picker = None;
                this.measures.retain(|row| row.id != id);
                this.changed(cx);
            },
            cx,
        ))
    }

    fn remove_button(
        &self,
        id: impl Into<ElementId>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.dark);
        controls::icon_button(id, c, !self.busy)
            .size(px(21.))
            .child(icons::glyph(Icon::Close, 12., self.dark))
            .tooltip(icons::tooltip(t("Remove"), self.dark))
            .when(!self.busy, |el| {
                el.cursor_pointer()
                    .hover(|el| el.bg(rgb(c.danger_bg)))
                    .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
            })
    }

    fn render_group(&self, row: &GroupRow, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let id = row.id;
        let detail = self
            .fields
            .iter()
            .find(|field| field.column.name == row.field)
            .map_or("", |field| field.column.data_type.as_str());
        let temporal = supports_bucket(detail);
        let c = Colors::new(self.dark);
        self.ordered_row(RowSection::Group, id, &row.field, cx)
            .child(
                self.field_selector(
                    ("aggregate-group-field", id),
                    Picker::GroupField(id),
                    Some(&row.field),
                    detail,
                    cx,
                )
                .w(gpui::relative(0.32))
                .flex_shrink_0(),
            )
            .when(temporal, |el| {
                let bucket_selector = if row.buckets.is_empty() {
                    let label = t("Choose time grouping");
                    self.selector(
                        ("aggregate-bucket", id),
                        Picker::Bucket(id),
                        label.clone(),
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(rgb(c.muted))
                            .child(label),
                        cx,
                    )
                } else {
                    self.selected_options(
                        ("aggregate-bucket", id),
                        Picker::Bucket(id),
                        row.buckets.iter().map(|bucket| {
                            let full = t(bucket.map_or("Exact value", bucket_label));
                            let short = bucket.map(|bucket| t(bucket_short_label(bucket)));
                            (short.unwrap_or_else(|| full.clone()), full)
                        }),
                        cx,
                    )
                };
                el.child(bucket_selector.flex_1())
            })
            .when(!temporal, |el| el.child(div().flex_1()))
            .child(self.remove_button(
                ("aggregate-remove-group", id),
                move |this, cx| {
                    this.picker = None;
                    this.groups.retain(|row| row.id != id);
                    this.changed(cx);
                },
                cx,
            ))
    }

    fn render_controls(&self, cx: &Context<Self>) -> gpui::Div {
        let c = Colors::new(self.dark);
        div()
            .flex_shrink_0()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                control_card(c)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(t("Group by")),
                            )
                            .child(self.field_selector(
                                "aggregate-add-group",
                                Picker::Groups,
                                Some(&t("Add group field")),
                                "",
                                cx,
                            )),
                    )
                    .when(self.groups.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(c.muted))
                                .child(t("No grouping (whole result)")),
                        )
                    })
                    .when(!self.groups.is_empty(), |el| {
                        el.child(row_labels("Group by", "Time grouping", c))
                    })
                    .children(self.groups.iter().map(|row| self.render_group(row, cx))),
            )
            .child(
                control_card(c)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(t("Measures")),
                            )
                            .child(
                                div()
                                    .id("aggregate-measure-help")
                                    .flex_shrink_0()
                                    .child(icons::glyph(Icon::Info, 13., self.dark))
                                    .tooltip(icons::tooltip(
                                        format!(
                                            "{}\n{}",
                                            t("Count and nunique exclude NULL."),
                                            t("StdDev and percentiles use Float64. Exact percentiles use DataFusion interpolation. Missing ratio measures NULL as a fraction.")
                                        ),
                                        self.dark,
                                    )),
                            )
                            .child(div().flex_1())
                            .child(self.button(
                                "aggregate-add-measure",
                                "Add measure",
                                !self.busy,
                                false,
                                |this, _, cx| {
                                    this.push_measure();
                                    this.changed(cx);
                                },
                                cx,
                            )),
                    )
                    .when(!self.measures.is_empty(), |el| {
                        el.child(row_labels("Summary field", "Summary method", c))
                    })
                    .children(self.measures.iter().map(|row| self.render_measure(row, cx))),
            )
    }

    fn picker_anchor(&self, picker: Picker, cx: &Context<Self>) -> gpui::Div {
        // Keep menus outside the controls' scroll clip.
        div()
            .absolute()
            .top(gpui::relative(1.))
            .left_0()
            .size(px(0.))
            .child(deferred(
                anchored()
                    .anchor(Corner::TopLeft)
                    .offset(point(px(0.), px(4.)))
                    .child(self.render_picker(picker, cx)),
            ))
    }

    fn render_picker(&self, picker: Picker, cx: &Context<Self>) -> gpui::AnyElement {
        let c = Colors::new(self.dark);
        let options = match picker {
            Picker::Groups | Picker::GroupField(_) | Picker::Field(_) => self.matching_fields.len(),
            Picker::Operator(id) => self.ops_for(id).len(),
            Picker::Bucket(_) => BUCKETS.len(),
        };
        let height = options.clamp(1, PICKER_VISIBLE_ROWS) as f32 * PICKER_ROW_HEIGHT;
        controls::menu_surface("aggregate-field-dropdown", c)
            .occlude()
            .flex()
            .flex_col()
            .gap_2()
            .w(px((self.viewport_width - 26.).clamp(160., PICKER_MAX_WIDTH)))
            .p_2()
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.picker == Some(picker) {
                        let editor = cx.weak_entity();
                        // Let the click select a different control before dismissing.
                        window.defer(cx, move |window, cx| {
                            let _ = editor.update(cx, |this, cx| {
                                if this.picker == Some(picker) {
                                    if this.focus.contains_focused(window, cx) {
                                        this.close_picker(window, cx);
                                    } else {
                                        this.dismiss_picker(cx);
                                    }
                                }
                            });
                        });
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t(picker.label())),
                    )
                    .when(picker.multiple(), |el| {
                        el.child(self.button(
                            "aggregate-picker-done",
                            "Done",
                            true,
                            false,
                            |this, window, cx| this.close_picker(window, cx),
                            cx,
                        ))
                    }),
            )
            .when(picker.is_field(), |el| {
                el.child(div().h(px(28.)).child(self.search.clone())).child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(c.muted))
                        .child(t("Struct and List are not supported.")),
                )
            })
            .when(matches!(picker, Picker::Bucket(_)), |el| {
                el.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(c.muted))
                        .whitespace_normal()
                        .child(t(
                            "Weeks start on Monday. Timestamp grouping uses the column time zone.",
                        )),
                )
            })
            .when(options == 0, |el| {
                el.child(
                    div()
                        .p_2()
                        .text_color(rgb(c.muted))
                        .child(t("No matching fields")),
                )
            })
            .when(options > 0, |el| {
                el.child(
                    uniform_list(
                        "aggregate-picker-options",
                        options,
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            range
                                .filter_map(|index| {
                                    let option = match picker {
                                        Picker::Groups
                                        | Picker::GroupField(_)
                                        | Picker::Field(_) => {
                                            PickerOption::Field(*this.matching_fields.get(index)?)
                                        }
                                        Picker::Operator(id) => {
                                            PickerOption::Operator(*this.ops_for(id).get(index)?)
                                        }
                                        Picker::Bucket(_) => {
                                            PickerOption::Bucket(*BUCKETS.get(index)?)
                                        }
                                    };
                                    Some(this.render_picker_option(picker, index, option, cx))
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .w_full()
                    .h(px(height))
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(rgb(c.border))
                    .track_scroll(self.picker_scroll.clone()),
                )
            })
            .into_any_element()
    }

    fn render_picker_option(
        &self,
        picker: Picker,
        index: usize,
        option: PickerOption,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let c = Colors::new(self.dark);
        let (name, detail, selected) = match (picker, option) {
            (
                Picker::Groups | Picker::GroupField(_) | Picker::Field(_),
                PickerOption::Field(field),
            ) => {
                let column = field
                    .and_then(|index| self.fields.get(index))
                    .map(|field| &field.column);
                let selected = match picker {
                    Picker::Groups => column.is_some_and(|column| {
                        self.groups.iter().any(|row| row.field == column.name)
                    }),
                    Picker::GroupField(id) => column.is_some_and(|column| {
                        self.groups
                            .iter()
                            .any(|row| row.id == id && row.field == column.name)
                    }),
                    Picker::Field(id) => self.measures.iter().any(|row| {
                        row.id == id
                            && match column {
                                Some(column) => row.source.column() == Some(column.name.as_str()),
                                None => row.source == MeasureSource::AllRecords,
                            }
                    }),
                    _ => false,
                };
                let (name, detail) = column.map_or_else(
                    || (t("All records"), String::new()),
                    |column| (column.name.clone(), column.data_type.clone()),
                );
                (name, detail, selected)
            }
            (Picker::Operator(id), PickerOption::Operator(op)) => (
                t(op.label()),
                String::new(),
                self.measures
                    .iter()
                    .any(|row| row.id == id && row.ops.contains(&op)),
            ),
            (Picker::Bucket(id), PickerOption::Bucket(bucket)) => (
                t(bucket.map_or("Exact value", bucket_label)),
                String::new(),
                self.groups
                    .iter()
                    .find(|row| row.id == id)
                    .is_some_and(|row| row.buckets.contains(&bucket)),
            ),
            _ => unreachable!("Option must belong to its picker"),
        };
        let hint = if detail.is_empty() {
            name.clone()
        } else {
            format!("{name} · {detail}")
        };
        controls::menu_item(("aggregate-picker-option", index), c, selected, true)
            .h(px(PICKER_ROW_HEIGHT))
            .w_full()
            .when(picker.multiple(), |el| {
                el.child(icons::checkbox(selected, self.dark))
            })
            .child(div().flex_1().min_w_0().truncate().child(name))
            .when(!detail.is_empty(), |el| {
                el.child(
                    div()
                        .max_w(gpui::relative(0.38))
                        .truncate()
                        .text_right()
                        .text_size(px(10.))
                        .text_color(rgb(c.muted))
                        .child(detail),
                )
            })
            .when(selected && !picker.multiple(), |el| {
                el.child(icons::glyph(Icon::Check, 13., self.dark))
            })
            .tooltip(icons::tooltip(hint, self.dark))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_picker_option(picker, option, window, cx)
            }))
            .into_any_element()
    }

    fn select_picker_option(
        &mut self,
        picker: Picker,
        option: PickerOption,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.picker != Some(picker) {
            return;
        }
        match (picker, option) {
            (_, PickerOption::Field(field)) => {
                self.select_field(picker, field, window, cx);
                return;
            }
            (Picker::Operator(id), PickerOption::Operator(op)) => {
                if !self.ops_for(id).contains(&op) {
                    return;
                }
                if let Some(row) = self.measures.iter_mut().find(|row| row.id == id) {
                    if let Some(index) = row.ops.iter().position(|selected| *selected == op) {
                        row.ops.remove(index);
                    } else {
                        row.ops.push(op);
                    }
                }
            }
            (Picker::Bucket(id), PickerOption::Bucket(bucket)) => {
                if let Some(row) = self.groups.iter_mut().find(|row| row.id == id) {
                    row.toggle_bucket(bucket);
                }
            }
            _ => return,
        }
        self.changed(cx);
    }

    pub(crate) fn set_viewport_width(&mut self, width: f32) {
        self.viewport_width = (width - 2.).max(1.);
    }

    fn result_columns(&self) -> Range<usize> {
        let count = self
            .result
            .as_ref()
            .map_or(0, |result| result.columns.len());
        let offset = -f32::from(self.horizontal.offset().x);
        let max_offset = (count as f32 * CELL_WIDTH - self.viewport_width).max(0.);
        visible_columns(count, offset.min(max_offset), self.viewport_width)
    }

    fn scroll_columns(&mut self, right: bool, cx: &mut Context<Self>) {
        let old = self.horizontal.offset();
        let maximum = f32::from(self.horizontal.max_offset().width);
        let x =
            (f32::from(old.x) + if right { -CELL_WIDTH } else { CELL_WIDTH }).clamp(-maximum, 0.);
        self.horizontal.set_offset(point(px(x), old.y));
        cx.notify();
    }

    fn render_result_row(
        &self,
        row: usize,
        columns: Range<usize>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let c = Colors::new(self.dark);
        let Some(result) = &self.result else {
            return div().into_any_element();
        };
        let count = result.columns.len();
        let active = self.detail_group == Some(row);
        let revision = self.result_revision;
        div()
            .id(("aggregate-result-row", row))
            .h(px(CELL_HEIGHT))
            .flex()
            .items_center()
            .bg(rgb(if active {
                c.selected
            } else if row % 2 == 0 {
                c.surface
            } else {
                c.stripe
            }))
            .border_b_1()
            .border_color(rgb(c.grid))
            .when(
                self.result_group_count > 0 && !self.dirty && !self.busy,
                |el| {
                    el.cursor_pointer()
                        .hover(move |el| el.bg(rgb(c.hover)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.result_revision == revision && this.detail_group != Some(row) {
                                cx.emit(AggregationEvent::DrillDown(row));
                            }
                        }))
                },
            )
            .child(
                div()
                    .w(px(columns.start as f32 * CELL_WIDTH))
                    .flex_shrink_0(),
            )
            .children(columns.clone().map(|column| {
                // ArrowTable display_text itself caps source Text; formatting
                // here is bounded to visible cells and a small glyph budget.
                let is_null = result.data.is_null(row, column);
                let text = clip_display(&result.data.display_text(row, column));
                div()
                    .w(px(CELL_WIDTH))
                    .h_full()
                    .px_2()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(rgb(c.grid))
                    .overflow_hidden()
                    .text_color(rgb(if is_null { c.null_text } else { c.text }))
                    .child(div().w_full().truncate().child(text))
            }))
            .child(
                div()
                    .w(px(count.saturating_sub(columns.end) as f32 * CELL_WIDTH))
                    .flex_shrink_0(),
            )
            .into_any_element()
    }

    fn render_results(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let c = Colors::new(self.dark);
        let Some(result) = &self.result else {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .py_3()
                .gap_2()
                .px_3()
                .text_color(rgb(c.muted))
                .child(icons::glyph(Icon::Table, 22., self.dark))
                .child(t(if self.busy {
                    "Aggregating…"
                } else {
                    "No aggregation results"
                }))
                .child(div().text_size(px(11.)).whitespace_normal().child(t(
                    if self.source_changed {
                        "Current result changed. Run aggregation again."
                    } else {
                        "Choose grouping fields and measures, then run."
                    },
                )))
                .into_any_element();
        };
        let columns = self.result_columns();
        let count = result.columns.len();
        let rows = result.data.row_count();
        let width = count as f32 * CELL_WIDTH;
        let table_height = 42. + CELL_HEIGHT * rows.clamp(1, 8) as f32;
        div()
            .flex_shrink_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(c.muted))
                    .child(format!(
                        "{} {} · {} {} · {} ms",
                        result.input_rows,
                        t("input rows"),
                        rows,
                        t("groups"),
                        result.elapsed_ms
                    )),
            )
            .when(
                self.result_group_count > 0
                    && rows > 0
                    && !self.busy
                    && !self.dirty
                    && !self.detail_open,
                |el| {
                    el.child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(c.muted))
                            .child(t("Click a group to show its detail rows.")),
                    )
                },
            )
            .when(self.dirty, |el| {
                el.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(c.warning))
                        .whitespace_normal()
                        .child(t("Aggregation request changed. Run again.")),
                )
            })
            .child(
                div()
                    .id("aggregate-result-horizontal")
                    .h(px(table_height))
                    .flex_shrink_0()
                    .min_h_0()
                    .overflow_x_scroll()
                    .track_scroll(&self.horizontal)
                    .border_1()
                    .border_color(rgb(c.border))
                    .bg(rgb(c.surface))
                    .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                    .child(
                        div()
                            .w(px(width))
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(40.))
                                    .flex_shrink_0()
                                    .flex()
                                    .bg(rgb(c.header))
                                    .border_b_1()
                                    .border_color(rgb(c.border))
                                    .child(
                                        div()
                                            .w(px(columns.start as f32 * CELL_WIDTH))
                                            .flex_shrink_0(),
                                    )
                                    .children(columns.clone().map(|index| {
                                        let column = &result.columns[index];
                                        div()
                                            .id(("aggregate-result-header", index))
                                            .w(px(CELL_WIDTH))
                                            .h_full()
                                            .flex_shrink_0()
                                            .px_2()
                                            .py_1()
                                            .flex()
                                            .flex_col()
                                            .overflow_hidden()
                                            .border_r_1()
                                            .border_color(rgb(c.grid))
                                            .child(
                                                div()
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .truncate()
                                                    .child(column.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(9.))
                                                    .text_color(rgb(c.muted))
                                                    .truncate()
                                                    .child(column.data_type.clone()),
                                            )
                                            .tooltip(icons::tooltip(
                                                format!("{} · {}", column.name, column.data_type),
                                                self.dark,
                                            ))
                                    }))
                                    .child(
                                        div()
                                            .w(px(count.saturating_sub(columns.end) as f32
                                                * CELL_WIDTH))
                                            .flex_shrink_0(),
                                    ),
                            )
                            .when(rows > 0, |el| {
                                el.child(
                                    uniform_list(
                                        "aggregate-result-rows",
                                        rows,
                                        cx.processor(|this, range: Range<usize>, _, cx| {
                                            let columns = this.result_columns();
                                            range
                                                .map(|row| {
                                                    this.render_result_row(row, columns.clone(), cx)
                                                })
                                                .collect()
                                        }),
                                    )
                                    .w(px(width))
                                    .flex_1()
                                    .min_h_0()
                                    .track_scroll(self.result_scroll.clone()),
                                )
                            })
                            .when(rows == 0, |el| {
                                el.child(
                                    div()
                                        .p_3()
                                        .text_color(rgb(c.muted))
                                        .child(t("No aggregation results")),
                                )
                            }),
                    ),
            )
            .when(count > 1, |el| {
                el.child(
                    div()
                        .h(px(24.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(self.button(
                            "aggregate-scroll-left",
                            "‹",
                            f32::from(self.horizontal.offset().x) < -0.5,
                            false,
                            |this, _, cx| this.scroll_columns(false, cx),
                            cx,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(10.))
                                .text_color(rgb(c.muted))
                                .child(format!(
                                    "{}–{} / {}",
                                    columns.start + 1,
                                    columns.end,
                                    count
                                )),
                        )
                        .child(self.button(
                            "aggregate-scroll-right",
                            "›",
                            -f32::from(self.horizontal.offset().x) + self.viewport_width
                                < width - 0.5,
                            false,
                            |this, _, cx| this.scroll_columns(true, cx),
                            cx,
                        )),
                )
            })
            .into_any_element()
    }
}

impl Focusable for AggregationEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AggregationEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::app::statistics_sidebar::section_header;

        let c = Colors::new(self.dark);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .track_focus(&self.focus)
            .text_color(rgb(c.text))
            .text_size(px(12.))
            .on_action(cx.listener(|this, _: &crate::app::Dismiss, window, cx| {
                this.hide(window, cx);
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.hide(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(c.muted))
                    .whitespace_normal()
                    .child(t("Aggregation uses the current result rows.")),
            )
            .child(self.render_controls(cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .when(self.detail_open, |el| {
                        el.child(self.button(
                            "aggregate-restore-details",
                            "Back to all details",
                            !self.busy,
                            false,
                            |_, _, cx| cx.emit(AggregationEvent::RestoreDetails),
                            cx,
                        ))
                    })
                    .child(div().flex_1())
                    .when(self.result.is_some(), |el| {
                        el.child(self.button(
                            "aggregate-export",
                            "Export summary…",
                            !self.busy && !self.dirty,
                            false,
                            |_, _, cx| cx.emit(AggregationEvent::Export),
                            cx,
                        ))
                    })
                    .child(self.button(
                        "aggregate-run",
                        if self.busy {
                            "Cancel"
                        } else {
                            "Run aggregation"
                        },
                        self.busy || !self.measures.is_empty(),
                        !self.busy,
                        |this, _, cx| {
                            if this.busy {
                                cx.emit(AggregationEvent::Cancel);
                            } else {
                                cx.emit(AggregationEvent::Run);
                            }
                        },
                        cx,
                    )),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(c.danger))
                        .whitespace_normal()
                        .child(t(error)),
                )
            })
            .child(
                section_header(
                    "aggregate-results-toggle",
                    "Summary results",
                    self.results_expanded,
                    c,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.results_expanded = !this.results_expanded;
                    cx.notify();
                })),
            )
            .when(self.results_expanded, |el| {
                el.child(self.render_results(cx))
            })
            .child(
                section_header(
                    "aggregate-charts-toggle",
                    "Summary charts",
                    self.charts_expanded,
                    c,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.charts_expanded = !this.charts_expanded;
                    cx.notify();
                })),
            )
            .when(self.charts_expanded, |el| {
                el.when_some(self.charts.as_ref(), |el, charts| {
                    el.child(charts.render(self.dark))
                })
            })
    }
}

fn control_card(c: Colors) -> gpui::Div {
    div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .p_2()
        .rounded(px(6.))
        .border_1()
        .border_color(rgb(c.border))
        .bg(rgb(c.surface))
}

fn row_labels(left: &'static str, right: &'static str, c: Colors) -> gpui::Div {
    div()
        .flex()
        .min_w_0()
        .pl(px(23.))
        .gap_1()
        .text_size(px(10.))
        .text_color(rgb(c.muted))
        .child(
            div()
                .w(gpui::relative(0.34))
                .min_w_0()
                .truncate()
                .child(t(left)),
        )
        .child(div().min_w_0().truncate().child(t(right)))
}

fn bucket_label(bucket: AggregateBucket) -> &'static str {
    match bucket {
        AggregateBucket::DayPart => "Day of month",
        AggregateBucket::WeekPart => "Week of year",
        AggregateBucket::MonthPart => "Month of year",
        AggregateBucket::QuarterPart => "Quarter of year",
        AggregateBucket::YearPart => "Calendar year",
        _ => bucket.label(),
    }
}

fn bucket_short_label(bucket: AggregateBucket) -> &'static str {
    match bucket {
        AggregateBucket::DayPart => "Day part",
        AggregateBucket::WeekPart => "Week part",
        AggregateBucket::MonthPart => "Month part",
        AggregateBucket::QuarterPart => "Quarter part",
        AggregateBucket::YearPart => "Year part",
        _ => bucket.label(),
    }
}

fn move_row<T>(rows: &mut Vec<T>, from: usize, to: usize, id: impl Fn(&T) -> usize) -> bool {
    let Some(from) = rows.iter().position(|row| id(row) == from) else {
        return false;
    };
    let Some(to) = rows.iter().position(|row| id(row) == to) else {
        return false;
    };
    if from == to {
        return false;
    }
    let row = rows.remove(from);
    rows.insert(to, row);
    true
}

fn visible_columns(count: usize, offset: f32, viewport: f32) -> Range<usize> {
    let offset = if offset.is_finite() {
        offset.max(0.)
    } else {
        0.
    };
    let viewport = if viewport.is_finite() && viewport > 0. {
        viewport
    } else {
        336.
    };
    let first = ((offset / CELL_WIDTH).floor() as usize).min(count);
    let end = (((offset + viewport) / CELL_WIDTH).ceil() as usize)
        .saturating_add(1)
        .min(count);
    first..end.max(first)
}

fn clip_display(text: &str) -> String {
    let mut chars = text.chars();
    let mut result = chars.by_ref().take(DISPLAY_CHARS).collect::<String>();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_grouping_requires_an_explicit_choice_and_can_be_cleared() {
        let mut row = GroupRow {
            id: 1,
            field: "order_date".into(),
            buckets: Vec::new(),
        };
        let mut request = AggregateRequest::default();
        assert!(
            row.append_to_request(true, &mut request)
                .unwrap_err()
                .to_string()
                .contains("order_date")
        );
        assert!(request.group_by.is_empty());

        row.toggle_bucket(Some(AggregateBucket::YearPart));
        row.toggle_bucket(Some(AggregateBucket::MonthPart));
        row.append_to_request(true, &mut request).unwrap();
        assert_eq!(request.group_by, ["order_date"]);
        assert_eq!(
            request.group_buckets,
            [
                ("order_date".into(), AggregateBucket::YearPart),
                ("order_date".into(), AggregateBucket::MonthPart),
            ]
        );
        row.toggle_bucket(Some(AggregateBucket::YearPart));
        row.toggle_bucket(Some(AggregateBucket::MonthPart));
        assert!(row.buckets.is_empty());
        assert!(row.append_to_request(true, &mut request).is_err());
    }

    #[test]
    fn exact_value_is_an_explicit_alternative_to_time_dimensions() {
        let mut row = GroupRow {
            id: 1,
            field: "order_date".into(),
            buckets: Vec::new(),
        };
        row.toggle_bucket(Some(AggregateBucket::YearPart));
        row.toggle_bucket(None);
        assert_eq!(row.buckets, [None]);
        let mut request = AggregateRequest::default();
        row.append_to_request(true, &mut request).unwrap();
        assert_eq!(request.group_by, ["order_date"]);
        assert!(request.group_buckets.is_empty());

        row.toggle_bucket(None);
        assert!(row.buckets.is_empty());
        row.toggle_bucket(None);
        row.toggle_bucket(Some(AggregateBucket::YearPart));
        assert_eq!(row.buckets, [Some(AggregateBucket::YearPart)]);

        let ordinary = GroupRow {
            id: 2,
            field: "category".into(),
            buckets: Vec::new(),
        };
        ordinary.append_to_request(false, &mut request).unwrap();
        assert_eq!(request.group_by, ["order_date", "category"]);
        assert!(request.group_buckets.is_empty());
    }

    #[test]
    fn blank_measure_cannot_implicitly_count_all_records() {
        let mut measure = MeasureRow::new(1);
        let mut metrics = Vec::new();

        assert!(measure.append_metrics(&mut metrics).is_err());
        measure.source = MeasureSource::AllRecords;
        assert!(measure.append_metrics(&mut metrics).is_err());
        assert!(metrics.is_empty());

        measure.ops.push(AggregateOp::CountRows);
        measure.append_metrics(&mut metrics).unwrap();
        assert_eq!(
            metrics,
            [AggregateMetric {
                field: None,
                op: AggregateOp::CountRows,
            }]
        );
    }

    #[test]
    fn dragging_orders_rows_by_stable_id_in_both_directions() {
        let mut rows = vec![3, 9, 15];
        assert!(move_row(&mut rows, 3, 15, |row| *row));
        assert_eq!(rows, [9, 15, 3]);
        assert!(move_row(&mut rows, 3, 9, |row| *row));
        assert_eq!(rows, [3, 9, 15]);
        assert!(!move_row(&mut rows, 9, 9, |row| *row));
        assert!(!move_row(&mut rows, 13, 9, |row| *row));
        assert_eq!(rows, [3, 9, 15]);
    }

    #[test]
    fn aggregate_column_viewport_is_bounded_and_clamps_unknown_geometry() {
        assert_eq!(visible_columns(0, 0., 0.), 0..0);
        assert_eq!(visible_columns(100_000, 0., 336.), 0..4);
        assert_eq!(visible_columns(100_000, 140_000., 336.), 1000..1004);
        assert_eq!(visible_columns(2, f32::INFINITY, f32::NAN), 0..2);
        assert_eq!(visible_columns(2, 100_000., 336.), 2..2);
    }
    #[test]
    fn aggregate_display_caps_unicode_without_changing_short_precise_values() {
        assert_eq!(
            clip_display("12345678901234567890.123456789012345678"),
            "12345678901234567890.123456789012345678"
        );
        assert_eq!(
            clip_display(&"中😀".repeat(100)).chars().count(),
            DISPLAY_CHARS + 1
        );
        assert!(clip_display(&"中😀".repeat(100)).ends_with('…'));
        assert_eq!(clip_display("NULL"), "NULL");
    }
}
