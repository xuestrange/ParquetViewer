//! A draft-only, typed filter builder. Applying emits a validated model; this
//! view never reads an Engine or mutates the parent's current query.
use crate::controls::{self, ButtonStyle};
use std::collections::HashMap;
use std::sync::Weak;

use gpui::{
    App, Context, Corner, ElementId, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Render, ScrollHandle, ScrollStrategy, Subscription, UniformListScrollHandle,
    Window, anchored, deferred, div, prelude::*, px, rgb, uniform_list,
};

use crate::{
    date_picker::{DatePicker, DatePickerEvent},
    engine::{ColumnInfo, QueryResult},
    filtering::{
        FilterCondition, FilterFieldKind, FilterGroup, FilterOperator, FilterTarget, MatchMode,
        field_kind, operator_label, operators, targets, value_kind, value_placeholder,
    },
    i18n::t,
    icons::{self, Icon, glyph},
    text_input::{InputEvent, TextInput},
    theme::Colors,
};

pub enum FilterEditorEvent {
    Apply(FilterGroup, bool),
    Cancel,
}

struct ConditionRow {
    id: usize,
    condition: FilterCondition,
    field_selected: bool,
    field_anchor: ScrollHandle,
    value: Entity<TextInput>,
    second_value: Entity<TextInput>,
    error: Option<String>,
    // Deleting a row drops its subscriptions; ordinary typing retains both
    // entities, their selections, undo histories and subscription identities.
    _subscriptions: Vec<Subscription>,
}

enum Picker {
    Field(FieldPicker),
    Operator(usize),
}

struct FieldPicker {
    row: usize,
    search: Entity<TextInput>,
    matches: Vec<usize>,
    highlighted: Option<usize>,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
}

struct CalendarEditor {
    row: usize,
    upper: bool,
    picker: Entity<DatePicker>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct CachedTarget {
    target: FilterTarget,
    kind: FilterFieldKind,
    value_kind: FilterFieldKind,
    operators: &'static [FilterOperator],
    labels: Vec<(FilterOperator, &'static str)>,
    is_struct: bool,
    search: String,
}

impl CachedTarget {
    fn new(target: FilterTarget) -> Self {
        let choices = operators(&target.data_type);
        Self {
            kind: if target.is_list {
                FilterFieldKind::List
            } else {
                field_kind(&target.data_type)
            },
            value_kind: if target.is_list && !choices.contains(&FilterOperator::Contains) {
                FilterFieldKind::NullOnly
            } else {
                value_kind(&target.data_type)
            },
            operators: choices,
            labels: choices
                .iter()
                .copied()
                .map(|operator| (operator, operator_label(operator, &target.data_type)))
                .collect(),
            is_struct: target
                .data_type
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("STRUCT("),
            search: format!(
                "{} {} {}.{}",
                target.label,
                target.data_type,
                target.field,
                target.path.join(".")
            )
            .to_lowercase(),
            target,
        }
    }
}

pub struct FilterEditor {
    columns: Vec<ColumnInfo>,
    loaded_result: Weak<QueryResult>,
    // Derived only when the schema changes. Rendering and input edits never
    // recurse through Struct schemas or inspect data values.
    targets: Vec<CachedTarget>,
    target_lookup: HashMap<String, HashMap<Vec<String>, usize>>,
    mode: MatchMode,
    whole_source: bool,
    rows: Vec<ConditionRow>,
    next_id: usize,
    rows_scroll: ScrollHandle,
    dark: bool,
    focus: FocusHandle,
    host_focus: Option<FocusHandle>,
    picker: Option<Picker>,
    calendar: Option<CalendarEditor>,
    calendar_focus: Option<(usize, bool)>,
    error: Option<String>,
}

impl EventEmitter<FilterEditorEvent> for FilterEditor {}

impl FilterEditor {
    pub fn whole_source(&self) -> bool {
        self.whole_source
    }

    pub fn set_whole_source(&mut self, whole_source: bool, cx: &mut Context<Self>) {
        self.whole_source = whole_source;
        cx.notify();
    }

    pub fn new(
        columns: Vec<ColumnInfo>,
        group: FilterGroup,
        dark: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let targets: Vec<_> = targets(&columns)
            .into_iter()
            .map(CachedTarget::new)
            .collect();
        let mut target_lookup: HashMap<String, HashMap<Vec<String>, usize>> = HashMap::new();
        for (index, entry) in targets.iter().enumerate() {
            target_lookup
                .entry(entry.target.field.clone())
                .or_default()
                .insert(entry.target.path.clone(), index);
        }
        let mut editor = Self {
            columns,
            loaded_result: Weak::new(),
            targets,
            target_lookup,
            mode: group.mode,
            whole_source: false,
            rows: Vec::new(),
            next_id: 0,
            rows_scroll: ScrollHandle::new(),
            dark,
            focus: cx.focus_handle(),
            host_focus: None,
            picker: None,
            calendar: None,
            calendar_focus: None,
            error: None,
        };
        for condition in group.conditions {
            editor.push_row(Some(condition), cx);
        }
        editor
    }

    pub fn set_loaded_result(&mut self, result: Weak<QueryResult>) {
        self.loaded_result = result;
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        if let Some(picker) = self.field_picker() {
            picker
                .search
                .update(cx, |input, cx| input.set_dark(dark, cx));
        }
        for row in &self.rows {
            for input in [&row.value, &row.second_value] {
                input.update(cx, |input, cx| input.set_dark(dark, cx));
            }
        }
        if let Some(calendar) = &self.calendar {
            calendar
                .picker
                .update(cx, |picker, cx| picker.set_dark(dark, cx));
        }
        cx.notify();
    }

    /// Refresh only application placeholders; drafts, undo state and field
    /// names remain unchanged when the application's language changes.
    pub fn refresh_language(&mut self, cx: &mut Context<Self>) {
        if let Some(picker) = self.field_picker() {
            picker.search.update(cx, |input, cx| {
                input.set_placeholder(&t("Search fields"), cx)
            });
        }
        for row in &self.rows {
            let data_type = self.value_data_type(row);
            let value = Self::value_placeholder(data_type, false);
            let upper = Self::value_placeholder(data_type, true);
            row.value
                .update(cx, |input, cx| input.set_placeholder(&value, cx));
            row.second_value
                .update(cx, |input, cx| input.set_placeholder(&upper, cx));
        }
        cx.notify();
    }

    pub fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.field_picker()
            .is_some_and(|picker| picker.search.focus_handle(cx).is_focused(window))
            || self.calendar.as_ref().is_some_and(|calendar| {
                let picker = calendar.picker.read(cx);
                picker.focus_handle(cx).is_focused(window) || picker.input_focused(window, cx)
            })
            || self.rows.iter().any(|row| {
                row.value.focus_handle(cx).is_focused(window)
                    || row.second_value.focus_handle(cx).is_focused(window)
            })
    }

    pub fn set_host_focus(&mut self, focus: FocusHandle) {
        self.host_focus = Some(focus);
    }

    pub fn matches_columns(&self, columns: &[ColumnInfo]) -> bool {
        self.columns.len() == columns.len()
            && self.columns.iter().zip(columns).all(|(current, next)| {
                current.name == next.name && current.data_type == next.data_type
            })
    }

    /// Read current input values for saved views. An empty row has no field
    /// path to persist, even though it counts as a pending change in the editor.
    pub fn snapshot_group(&self, cx: &App) -> FilterGroup {
        FilterGroup {
            mode: self.mode,
            conditions: self
                .rows
                .iter()
                .filter(|row| row.field_selected)
                .map(|row| {
                    let mut condition = row.condition.clone();
                    condition.value = row.value.read(cx).text();
                    condition.second_value = row.second_value.read(cx).text();
                    condition
                })
                .collect(),
        }
    }

    pub fn has_pending_changes(&self, group: &FilterGroup, whole_source: bool, cx: &App) -> bool {
        self.whole_source != whole_source
            || self.rows.iter().any(|row| !row.field_selected)
            || self.snapshot_group(cx) != *group
    }

    fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.calendar = None;
        self.calendar_focus = None;
        if self.field_picker().is_some() {
            self.picker = None;
        }
        if let Some(focus) = &self.host_focus {
            focus.focus(window);
        }
        cx.emit(FilterEditorEvent::Cancel);
    }

    pub fn focus_first(&self, window: &mut Window, cx: &App) {
        if let Some(calendar) = &self.calendar {
            calendar.picker.focus_handle(cx).focus(window);
        } else if let Some(picker) = self.field_picker() {
            picker.search.focus_handle(cx).focus(window);
        } else if self.picker.is_some() {
            self.focus.focus(window);
        } else if let Some((index, row)) = self
            .rows
            .iter()
            .enumerate()
            .find(|(_, row)| self.has_text_input(row))
        {
            self.rows_scroll.scroll_to_item(index);
            row.value.focus_handle(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
    }

    fn push_row(&mut self, condition: Option<FilterCondition>, cx: &mut Context<Self>) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let data_type = condition
            .as_ref()
            .and_then(|condition| self.target_for(condition))
            .map_or("", |target| target.target.value_type.as_str());
        let value_placeholder = Self::value_placeholder(data_type, false);
        let upper_placeholder = Self::value_placeholder(data_type, true);
        let field_selected = condition.is_some();
        let condition = condition.unwrap_or_default();
        let value = cx.new(|cx| {
            let mut input = TextInput::new(cx)
                .with_dark(self.dark)
                .with_placeholder(&value_placeholder);
            input.set_text(&condition.value, cx);
            input
        });
        let second_value = cx.new(|cx| {
            let mut input = TextInput::new(cx)
                .with_dark(self.dark)
                .with_placeholder(&upper_placeholder);
            input.set_text(&condition.second_value, cx);
            input
        });
        let subscriptions = [(value.clone(), false), (second_value.clone(), true)]
            .into_iter()
            .map(|(input, second)| {
                cx.subscribe(&input, move |this, input, event, cx| match event {
                    InputEvent::Changed => {
                        let text = input.read(cx).text();
                        if let Some(row) = this.rows.iter_mut().find(|row| row.id == id) {
                            if second {
                                row.condition.second_value = text;
                            } else {
                                row.condition.value = text;
                            }
                            row.error = None;
                        }
                        this.error = None;
                        cx.notify();
                    }
                    InputEvent::Submit => this.apply(cx),
                })
            })
            .collect();
        self.rows.push(ConditionRow {
            id,
            condition,
            field_selected,
            field_anchor: ScrollHandle::new(),
            value,
            second_value,
            error: None,
            _subscriptions: subscriptions,
        });
        id
    }

    fn add_condition(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.calendar = None;
        self.calendar_focus = None;
        if self.targets.is_empty() {
            return;
        }
        let id = self.push_row(None, cx);
        self.picker = None;
        self.error = None;
        self.rows_scroll.scroll_to_bottom();
        self.focus_row(id, window, cx);
        cx.notify();
    }

    fn focus_row(&self, id: usize, window: &mut Window, cx: &App) {
        if let Some((index, row)) = self.rows.iter().enumerate().find(|(_, row)| row.id == id) {
            self.rows_scroll.scroll_to_item(index);
            if self.has_text_input(row) {
                row.value.focus_handle(cx).focus(window);
            } else {
                self.focus.focus(window);
            }
        } else {
            self.focus.focus(window);
        }
    }

    fn value_placeholder(data_type: &str, upper: bool) -> String {
        t(value_placeholder(data_type, upper))
    }

    fn target_for(&self, condition: &FilterCondition) -> Option<&CachedTarget> {
        let index = self
            .target_lookup
            .get(&condition.field)?
            .get(&condition.path)?;
        self.targets.get(*index)
    }

    fn row_target(&self, row: &ConditionRow) -> Option<&CachedTarget> {
        if row.field_selected {
            self.target_for(&row.condition)
        } else {
            None
        }
    }

    fn value_data_type(&self, row: &ConditionRow) -> &str {
        self.row_target(row)
            .map_or("", |entry| entry.target.value_type.as_str())
    }

    fn input_kind(&self, row: &ConditionRow) -> FilterFieldKind {
        self.row_target(row)
            .map_or(FilterFieldKind::NullOnly, |entry| entry.value_kind)
    }

    fn is_boolean(&self, row: &ConditionRow) -> bool {
        self.input_kind(row) == FilterFieldKind::Boolean
    }

    fn has_text_input(&self, row: &ConditionRow) -> bool {
        row.condition.operator.needs_value()
            && self
                .row_target(row)
                .is_some_and(|entry| entry.value_kind != FilterFieldKind::Boolean)
    }

    fn open_calendar(
        &mut self,
        id: usize,
        upper: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .calendar
            .as_ref()
            .is_some_and(|calendar| calendar.row == id && calendar.upper == upper)
        {
            self.calendar = None;
            self.focus_row(id, window, cx);
            cx.notify();
            return;
        }
        let Some(row) = self.rows.iter().find(|row| row.id == id) else {
            return;
        };
        let value = if upper {
            row.second_value.read(cx).text()
        } else {
            row.value.read(cx).text()
        };
        let seed = if value.trim().is_empty()
            && self
                .row_target(row)
                .is_some_and(|entry| !entry.target.is_list)
        {
            self.loaded_result.upgrade().and_then(|result| {
                result
                    .data
                    .first_calendar_date(&row.condition.field, &row.condition.path)
            })
        } else {
            None
        };
        let picker = cx.new(|cx| DatePicker::new(&value, seed, self.dark, cx));
        let subscription = cx.subscribe(&picker, move |this, _, event, cx| {
            if let DatePickerEvent::Select(date) = event
                && let Some(index) = this.rows.iter().position(|row| row.id == id)
            {
                let data_type = this.value_data_type(&this.rows[index]).to_owned();
                let input = if upper {
                    this.rows[index].second_value.clone()
                } else {
                    this.rows[index].value.clone()
                };
                let selected = calendar_value(&data_type, &input.read(cx).text(), *date);
                let row = &mut this.rows[index];
                if upper {
                    row.condition.second_value = selected.clone();
                } else {
                    row.condition.value = selected.clone();
                }
                input.update(cx, |input, cx| input.set_text(&selected, cx));
                row.error = None;
                this.error = None;
            }
            this.calendar = None;
            this.calendar_focus = Some((id, upper));
            cx.notify();
        });
        picker.focus_handle(cx).focus(window);
        self.calendar = Some(CalendarEditor {
            row: id,
            upper,
            picker,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn available_operators(&self, id: usize) -> &'static [FilterOperator] {
        self.rows
            .iter()
            .find(|row| row.id == id)
            .and_then(|row| self.row_target(row))
            .map_or(&[][..], |entry| entry.operators)
    }

    fn operator_label(&self, row: &ConditionRow, operator: FilterOperator) -> &'static str {
        self.row_target(row)
            .and_then(|entry| {
                entry
                    .labels
                    .iter()
                    .find_map(|(choice, label)| (*choice == operator).then_some(*label))
            })
            .unwrap_or_else(|| operator.label())
    }

    fn field_picker(&self) -> Option<&FieldPicker> {
        match &self.picker {
            Some(Picker::Field(picker)) => Some(picker),
            _ => None,
        }
    }

    fn open_field_picker(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.field_picker().is_some_and(|picker| picker.row == row) {
            self.close_picker(window, cx);
            return;
        }
        self.calendar = None;
        self.calendar_focus = None;
        let search = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(self.dark)
                .with_placeholder(&t("Search fields"))
        });
        let subscription =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                InputEvent::Changed => this.refresh_field_matches(cx),
                InputEvent::Submit => {
                    if let Some(picker) = this.field_picker()
                        && let Some(index) = picker.highlighted
                        && let Some(&target) = picker.matches.get(index)
                    {
                        this.choose_field(picker.row, target, window, cx);
                    }
                }
            });
        let highlighted = self
            .rows
            .iter()
            .find(|condition| condition.id == row)
            .filter(|row| row.field_selected)
            .and_then(|row| {
                self.target_lookup
                    .get(&row.condition.field)?
                    .get(&row.condition.path)
            })
            .copied();
        let scroll = UniformListScrollHandle::new();
        if let Some(index) = highlighted {
            scroll.scroll_to_item(index, ScrollStrategy::Center);
        }
        self.picker = Some(Picker::Field(FieldPicker {
            row,
            search: search.clone(),
            matches: (0..self.targets.len()).collect(),
            highlighted,
            scroll,
            _subscription: subscription,
        }));
        search.focus_handle(cx).focus(window);
        cx.notify();
    }

    fn refresh_field_matches(&mut self, cx: &mut Context<Self>) {
        let Some(Picker::Field(picker)) = &mut self.picker else {
            return;
        };
        let needle = picker.search.read(cx).text().trim().to_lowercase();
        picker.matches.clear();
        picker.matches.extend(
            self.targets
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    (needle.is_empty() || entry.search.contains(&needle)).then_some(index)
                }),
        );
        picker.highlighted = None;
        picker.scroll = UniformListScrollHandle::new();
        cx.notify();
    }

    fn move_field_highlight(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(Picker::Field(picker)) = &mut self.picker else {
            return;
        };
        if picker.matches.is_empty() {
            return;
        }
        let last = picker.matches.len() - 1;
        let index = match (picker.highlighted, down) {
            (None, true) => 0,
            (None, false) => last,
            (Some(index), true) => (index + 1).min(last),
            (Some(index), false) => index.saturating_sub(1),
        };
        picker.highlighted = Some(index);
        picker.scroll.scroll_to_item(index, ScrollStrategy::Top);
        cx.notify();
    }

    fn open_operator_picker(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.available_operators(id).is_empty() {
            return;
        }
        self.calendar = None;
        self.calendar_focus = None;
        self.picker = Some(Picker::Operator(id));
        self.focus.focus(window);
        cx.notify();
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.picker.take().map(|picker| match picker {
            Picker::Field(picker) => picker.row,
            Picker::Operator(id) => id,
        });
        if let Some(id) = id {
            self.focus_row(id, window, cx);
        }
        cx.notify();
    }

    fn choose_field(
        &mut self,
        id: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.targets.get(index).cloned() else {
            return;
        };
        let previous_kind = self
            .rows
            .iter()
            .find(|row| row.id == id)
            .and_then(|row| self.row_target(row))
            .map(|entry| entry.value_kind);
        let next_kind = entry.value_kind;
        self.calendar = None;
        if let Some(row) = self.rows.iter_mut().find(|row| row.id == id) {
            row.condition.field = entry.target.field.clone();
            row.condition.path = entry.target.path.clone();
            let value_placeholder = Self::value_placeholder(&entry.target.value_type, false);
            let upper_placeholder = Self::value_placeholder(&entry.target.value_type, true);
            row.value.update(cx, |input, cx| {
                input.set_placeholder(&value_placeholder, cx)
            });
            row.second_value.update(cx, |input, cx| {
                input.set_placeholder(&upper_placeholder, cx)
            });
            let choices = entry.operators;
            if !row.field_selected || !choices.contains(&row.condition.operator) {
                row.condition.operator = choices.first().copied().unwrap_or(FilterOperator::IsNull);
            }
            row.field_selected = true;
            if previous_kind != Some(next_kind) {
                row.condition.value = if next_kind == FilterFieldKind::Boolean {
                    "true".into()
                } else {
                    String::new()
                };
                row.condition.second_value.clear();
                row.value
                    .update(cx, |input, cx| input.set_text(&row.condition.value, cx));
                row.second_value
                    .update(cx, |input, cx| input.set_text("", cx));
            } else if next_kind == FilterFieldKind::Boolean
                && !matches!(row.condition.value.as_str(), "true" | "false")
            {
                row.condition.value = "true".into();
                row.value.update(cx, |input, cx| input.set_text("true", cx));
            }
            row.error = None;
        }
        self.error = None;
        self.close_picker(window, cx);
    }

    fn choose_operator(
        &mut self,
        id: usize,
        operator: FilterOperator,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.calendar = None;
        if let Some(row) = self.rows.iter_mut().find(|row| row.id == id) {
            row.condition.operator = operator;
            row.error = None;
        }
        self.error = None;
        self.close_picker(window, cx);
        self.focus_row(id, window, cx);
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        let mut invalid = false;
        let mut first_unselected = None;
        for (index, row) in self.rows.iter_mut().enumerate() {
            if !row.field_selected {
                row.error = Some("Choose an available field.".into());
                first_unselected.get_or_insert(index);
                continue;
            }
            row.condition.value = row.value.read(cx).text();
            row.condition.second_value = row.second_value.read(cx).text();
            row.error = FilterGroup {
                mode: self.mode,
                conditions: vec![row.condition.clone()],
            }
            .to_filter(&self.columns)
            .err()
            .map(|error| {
                error
                    .chain()
                    .skip(1)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(": ")
            });
            invalid |= row.error.is_some();
        }
        if let Some(index) = first_unselected {
            self.error = Some("Choose a field for each condition before applying filters.".into());
            self.rows_scroll.scroll_to_item(index);
            cx.notify();
            return;
        }
        let group = FilterGroup {
            mode: self.mode,
            conditions: self.rows.iter().map(|row| row.condition.clone()).collect(),
        };
        match group.to_filter(&self.columns) {
            Ok(_) if !invalid => {
                self.error = None;
                let whole_source = self.whole_source && !group.conditions.is_empty();
                self.whole_source = whole_source;
                cx.emit(FilterEditorEvent::Apply(group, whole_source));
            }
            Err(error) => self.error = Some(format!("{error:#}")),
            _ => {}
        }
        cx.notify();
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
        let icon = match label {
            "Apply" => Some(Icon::Check),
            "Add condition" => Some(Icon::Fields),
            "Clear" => Some(Icon::Clear),
            "Close" | "Remove" => Some(Icon::Close),
            "Back" => Some(Icon::Back),
            _ => None,
        };
        let style = if primary {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Standard
        };
        controls::button(id, c, style, enabled)
            .when_some(icon, |el, icon| {
                el.child(if primary {
                    icons::primary_glyph(icon, 14.)
                } else {
                    glyph(icon, 14., self.dark)
                })
            })
            .child(t(label))
            .tooltip(icons::tooltip(t(label), self.dark))
            .when(enabled, |el| {
                el.on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
            })
    }

    fn render_value_input(
        &self,
        row: &ConditionRow,
        upper: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let c = Colors::new(self.dark);
        let id = row.id;
        let calendar = matches!(
            self.input_kind(row),
            FilterFieldKind::Date | FilterFieldKind::DateTime
        );
        let input = if upper {
            row.second_value.clone()
        } else {
            row.value.clone()
        };
        div()
            .h(px(28.))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .child(div().flex_1().min_w_0().h_full().child(input))
            .when(calendar, |el| {
                el.child(
                    controls::icon_button(
                        (
                            if upper {
                                "filter-upper-calendar"
                            } else {
                                "filter-calendar"
                            },
                            id,
                        ),
                        c,
                        true,
                    )
                    .when(
                        self.calendar
                            .as_ref()
                            .is_some_and(|calendar| calendar.row == id && calendar.upper == upper),
                        |el| el.bg(rgb(c.selected)),
                    )
                    .child(glyph(Icon::Calendar, 15., self.dark))
                    .tooltip(icons::tooltip(t("Choose date"), self.dark))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_calendar(id, upper, window, cx)
                    })),
                )
            })
            .into_any_element()
    }

    fn render_row(
        &self,
        row: &ConditionRow,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let c = Colors::new(self.dark);
        let id = row.id;
        let entry = self.row_target(row);
        let field = entry.map_or_else(|| t("Select field"), |entry| entry.target.label.clone());
        let data_type = entry.map_or("", |entry| entry.target.data_type.as_str());
        let kind = entry.map_or(FilterFieldKind::NullOnly, |entry| entry.kind);
        let input_kind = self.input_kind(row);
        let label = self.operator_label(row, row.condition.operator);
        let available = entry.map_or(&[][..], |entry| entry.operators);
        let picker = self.field_picker().filter(|picker| picker.row == id);
        div()
            .id(("filter-row", id))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_2()
            .py_2()
            .border_b_1()
            .border_color(rgb(if row.error.is_some() {
                c.danger
            } else {
                c.grid
            }))
            .child(
                div()
                    .relative()
                    .w_full()
                    .min_w_0()
                    .child(
                        controls::selector(("filter-field", id), c, picker.is_some(), true)
                            .track_scroll(&row.field_anchor)
                            .w_full()
                            .when(!row.field_selected && row.error.is_some(), |el| {
                                el.border_color(rgb(c.danger))
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(entry.is_none(), |el| el.text_color(rgb(c.muted)))
                                    .child(field.clone()),
                            )
                            .when(entry.is_some(), |el| {
                                el.child(
                                    div()
                                        .max_w(px(112.))
                                        .min_w_0()
                                        .text_size(px(10.))
                                        .text_color(rgb(c.muted))
                                        .truncate()
                                        .child(data_type.to_owned()),
                                )
                            })
                            .child(controls::chevron(c, picker.is_some()))
                            .tooltip(icons::tooltip(
                                if entry.is_some() {
                                    format!("{field} · {} · {data_type}", t(kind.label()))
                                } else {
                                    field
                                },
                                self.dark,
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_field_picker(id, window, cx)
                            })),
                    )
                    .when_some(picker, |control, picker| {
                        control.child(self.render_field_picker(picker, row, window, cx))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(entry.is_some(), |el| {
                        el.child(
                            controls::selector(("filter-operator", id), c, false, true)
                                .flex_1()
                                .child(div().flex_1().min_w_0().truncate().child(t(label)))
                                .child(controls::chevron(c, false))
                                .tooltip(icons::tooltip(t(label), self.dark))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_operator_picker(id, window, cx)
                                })),
                        )
                    })
                    .child(self.button(
                        ("filter-remove", id),
                        "Remove",
                        true,
                        false,
                        move |this, window, cx| {
                            if this
                                .calendar
                                .as_ref()
                                .is_some_and(|calendar| calendar.row == id)
                            {
                                this.calendar = None;
                            }
                            this.rows.retain(|row| row.id != id);
                            this.error = None;
                            this.focus.focus(window);
                            cx.notify();
                        },
                        cx,
                    )),
            )
            .when(entry.is_some(), |el| {
                el.child(
                    controls::segment_group(c).flex_wrap().children(
                        quick_operators(kind)
                            .iter()
                            .copied()
                            .filter(|(operator, _)| available.contains(operator))
                            .enumerate()
                            .map(|(index, (operator, short))| {
                                let selected = row.condition.operator == operator;
                                let hint = self.operator_label(row, operator);
                                controls::segment(
                                    ("filter-quick-operator", id * 16 + index),
                                    c,
                                    selected,
                                )
                                .min_w(px(28.))
                                .text_size(px(11.))
                                .child(t(short))
                                .tooltip(icons::tooltip(t(hint), self.dark))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.choose_operator(id, operator, window, cx)
                                    },
                                ))
                            }),
                    ),
                )
            })
            .when(
                entry.is_some() && row.condition.operator.needs_value(),
                |el| {
                    if self.is_boolean(row) {
                        el.child(controls::segment_group(c).children(
                            [(true, "True"), (false, "False")].map(|(value, label)| {
                                let selected = row.condition.value.eq_ignore_ascii_case(if value {
                                    "true"
                                } else {
                                    "false"
                                });
                                controls::segment(
                                    (if value { "filter-true" } else { "filter-false" }, id),
                                    c,
                                    selected,
                                )
                                .child(t(label))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if let Some(row) =
                                            this.rows.iter_mut().find(|row| row.id == id)
                                        {
                                            let text = if value { "true" } else { "false" };
                                            row.condition.value = text.into();
                                            row.value
                                                .update(cx, |input, cx| input.set_text(text, cx));
                                            row.error = None;
                                        }
                                        this.error = None;
                                        cx.notify();
                                    },
                                ))
                            }),
                        ))
                    } else {
                        el.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(self.render_value_input(row, false, cx))
                                .when(row.condition.operator == FilterOperator::Between, |el| {
                                    el.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(c.muted))
                                            .child(t("and")),
                                    )
                                    .child(self.render_value_input(row, true, cx))
                                }),
                        )
                    }
                },
            )
            .when_some(
                self.calendar.as_ref().filter(|calendar| calendar.row == id),
                |el, calendar| el.child(calendar.picker.clone()),
            )
            .when(
                entry.is_some()
                    && (row.condition.operator.needs_value()
                        || matches!(kind, FilterFieldKind::NullOnly | FilterFieldKind::List)),
                |el| {
                    let hint = if kind == FilterFieldKind::List {
                        if !available.contains(&FilterOperator::Contains) {
                            "Check whether the List is empty or missing."
                        } else if input_kind == FilterFieldKind::Text {
                            "Match a whole List element; Text matching is case-sensitive."
                        } else {
                            "Match a whole List element, using its original Data type."
                        }
                    } else {
                        match input_kind {
                            FilterFieldKind::Number => {
                                "Enter a number; decimals and large integers stay exact."
                            }
                            FilterFieldKind::Text => "Text matching ignores case.",
                            FilterFieldKind::Boolean => "Choose true or false.",
                            FilterFieldKind::Date => "Choose a date or enter YYYY-MM-DD.",
                            FilterFieldKind::DateTime => {
                                "Choose a date and enter the time; include a timezone when shown."
                            }
                            FilterFieldKind::Time => "Enter a time in the format shown.",
                            FilterFieldKind::NullOnly
                                if entry.is_some_and(|entry| entry.is_struct) =>
                            {
                                "Select a Struct field to compare its value."
                            }
                            FilterFieldKind::NullOnly => {
                                "This field supports missing-value checks."
                            }
                            FilterFieldKind::List => "Check whether the List is empty or missing.",
                        }
                    };
                    el.child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(c.muted))
                            .whitespace_normal()
                            .child(t(hint)),
                    )
                },
            )
            .when_some(row.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(c.danger))
                        .whitespace_normal()
                        .child(t(error)),
                )
            })
            .into_any_element()
    }

    fn render_field_picker(
        &self,
        picker: &FieldPicker,
        row: &ConditionRow,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = Colors::new(self.dark);
        let id = row.id;
        let anchor_handle = row.field_anchor.clone();
        let anchor = anchor_handle.bounds();
        let rows = self.rows_scroll.bounds();
        let viewport = window.viewport_size();
        let gap = px(4.);
        let margin = px(8.);
        let count = picker.matches.len();
        let desired = px(42. + 32. * count.clamp(1, 7) as f32);
        let mut above = anchor.top() - rows.top().max(margin) - gap;
        let mut below = rows.bottom().min(viewport.height - margin) - anchor.bottom() - gap;
        // Stay within the condition list when possible. On a short window,
        // the popup can use more of the sidebar while keeping its trigger.
        if above.max(below) < px(74.) {
            above = anchor.top() - margin - gap;
            below = viewport.height - margin - anchor.bottom() - gap;
        }
        let opens_above = below < desired && above > below;
        let height = desired
            .min(if opens_above { above } else { below })
            .max(px(1.));
        let width = anchor
            .size
            .width
            .min(viewport.width - margin * 2.)
            .max(px(1.));
        let (corner, top) = if opens_above {
            (Corner::BottomLeft, -gap)
        } else {
            (Corner::TopLeft, px(28.) + gap)
        };

        let popup = controls::menu_surface("filter-field-popup", colors)
            .w(width)
            .h(height)
            .flex()
            .flex_col()
            .gap_1()
            .overflow_hidden()
            .occlude()
            .on_mouse_down_out(cx.listener(
                move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if !anchor_handle.bounds().contains(&event.position)
                        && this.field_picker().is_some_and(|picker| picker.row == id)
                    {
                        this.picker = None;
                        this.focus.focus(window);
                        cx.notify();
                    }
                },
            ))
            .capture_action(cx.listener(|this, _: &crate::text_input::Up, _, cx| {
                this.move_field_highlight(false, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &crate::text_input::Down, _, cx| {
                this.move_field_highlight(true, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .h(px(28.))
                    .flex_shrink_0()
                    .child(picker.search.clone()),
            )
            .when(count == 0, |popup| {
                popup.child(
                    div()
                        .p_2()
                        .text_color(rgb(colors.muted))
                        .child(t("No matching fields")),
                )
            })
            .when(count > 0, |popup| {
                popup.child(
                    uniform_list(
                        "filter-field-options",
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            let Some(picker) =
                                this.field_picker().filter(|picker| picker.row == id)
                            else {
                                return Vec::new();
                            };
                            let row = this.rows.iter().find(|row| row.id == id);
                            let colors = Colors::new(this.dark);
                            range
                                .filter_map(|index| {
                                    let target_index = *picker.matches.get(index)?;
                                    let target = &this.targets.get(target_index)?.target;
                                    let selected = row.is_some_and(|row| {
                                        row.field_selected
                                            && row.condition.field == target.field
                                            && row.condition.path == target.path
                                    });
                                    Some(
                                        controls::menu_item(
                                            ("filter-field-option", target_index),
                                            colors,
                                            selected,
                                            true,
                                        )
                                        .w_full()
                                        .h(px(32.))
                                        .when(picker.highlighted == Some(index), |el| {
                                            el.bg(rgb(colors.selected))
                                        })
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(12.))
                                                .child(target.label.clone()),
                                        )
                                        .child(
                                            div()
                                                .w(gpui::relative(0.40))
                                                .flex_shrink_0()
                                                .truncate()
                                                .text_right()
                                                .text_size(px(10.))
                                                .text_color(rgb(colors.muted))
                                                .child(target.data_type.clone()),
                                        )
                                        .tooltip(icons::tooltip(
                                            format!("{} · {}", target.label, target.data_type),
                                            this.dark,
                                        ))
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.choose_field(id, target_index, window, cx);
                                            }),
                                        ),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .track_scroll(picker.scroll.clone()),
                )
            });

        // Defer painting so the menu can overlap the scrollable condition list.
        // Its absolute position is relative to the field control in this row.
        deferred(
            div()
                .absolute()
                .left_0()
                .top(top)
                .child(anchored().anchor(corner).child(popup)),
        )
    }

    fn render_operator_picker(&self, id: usize, cx: &Context<Self>) -> gpui::AnyElement {
        let c = Colors::new(self.dark);
        let count = self.available_operators(id).len();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(self.button(
                        "filter-picker-back",
                        "Back",
                        true,
                        false,
                        |this, window, cx| this.close_picker(window, cx),
                        cx,
                    ))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(t("Select operator")),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_b_1()
                    .border_color(rgb(c.border))
                    .overflow_hidden()
                    .bg(rgb(c.surface))
                    .when(count > 0, |el| {
                        el.child(
                            uniform_list(
                                "filter-picker-options",
                                count,
                                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                                    let row = this.rows.iter().find(|row| row.id == id);
                                    let c = Colors::new(this.dark);
                                    range
                                        .filter_map(|index| {
                                            let operator =
                                                *this.available_operators(id).get(index)?;
                                            let name = t(row.map_or_else(
                                                || operator.label(),
                                                |row| this.operator_label(row, operator),
                                            ));
                                            let selected = row.is_some_and(|row| {
                                                row.condition.operator == operator
                                            });
                                            Some(
                                                controls::menu_item(
                                                    ("filter-picker-option", index),
                                                    c,
                                                    selected,
                                                    true,
                                                )
                                                .w_full()
                                                .h(px(36.))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .child(name.clone()),
                                                )
                                                .tooltip(icons::tooltip(name, this.dark))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.choose_operator(
                                                            id, operator, window, cx,
                                                        );
                                                    },
                                                )),
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .size_full(),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Focusable for FilterEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FilterEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((id, upper)) = self.calendar_focus.take()
            && let Some(row) = self.rows.iter().find(|row| row.id == id)
        {
            let focus = if upper {
                row.second_value.focus_handle(cx)
            } else {
                row.value.focus_handle(cx)
            };
            window.defer(cx, move |window, _| focus.focus(window));
        }
        let c = Colors::new(self.dark);
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .track_focus(&self.focus)
            .text_size(px(12.))
            .text_color(rgb(c.text))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_color(rgb(c.muted)).child(t("Filter scope")))
                    .child(
                        div().flex().gap_2().children(
                            [(false, "Loaded range"), (true, "Entire file")]
                                .into_iter()
                                .map(|(whole, label)| {
                                    self.button(
                                        ("filter-scope", usize::from(whole)),
                                        label,
                                        true,
                                        self.whole_source == whole,
                                        move |this, _, cx| this.set_whole_source(whole, cx),
                                        cx,
                                    )
                                }),
                        ),
                    )
                    .when(self.whole_source, |scope| {
                        scope.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(c.muted))
                                .child(t("Scan the entire source and import only matching rows.")),
                        )
                    }),
            )
            .on_action(cx.listener(|this, _: &crate::app::Dismiss, window, cx| {
                if let Some(calendar) = this.calendar.take() {
                    this.calendar_focus = Some((calendar.row, calendar.upper));
                    cx.notify();
                } else if this.field_picker().is_some() {
                    this.close_picker(window, cx);
                } else {
                    this.hide(window, cx);
                }
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if let Some(calendar) = this.calendar.take() {
                        this.calendar_focus = Some((calendar.row, calendar.upper));
                        cx.notify();
                    } else if this.field_picker().is_some() {
                        this.close_picker(window, cx);
                    } else {
                        this.hide(window, cx);
                    }
                    cx.stop_propagation();
                } else if event.keystroke.key == "tab" {
                    let inputs = if let Some(picker) = this.field_picker() {
                        vec![(None, picker.search.focus_handle(cx))]
                    } else if this.picker.is_some() {
                        Vec::new()
                    } else {
                        this.rows
                            .iter()
                            .enumerate()
                            .filter(|(_, row)| this.has_text_input(row))
                            .flat_map(|(index, row)| {
                                let mut inputs = vec![(Some(index), row.value.focus_handle(cx))];
                                if row.condition.operator == FilterOperator::Between {
                                    inputs.push((Some(index), row.second_value.focus_handle(cx)));
                                }
                                inputs
                            })
                            .collect::<Vec<_>>()
                    };
                    if !inputs.is_empty() {
                        let current = inputs
                            .iter()
                            .position(|(_, input)| input.is_focused(window));
                        let next = if event.keystroke.modifiers.shift {
                            current.map_or(inputs.len() - 1, |index| {
                                (index + inputs.len() - 1) % inputs.len()
                            })
                        } else {
                            current.map_or(0, |index| (index + 1) % inputs.len())
                        };
                        if let Some(index) = inputs[next].0 {
                            this.rows_scroll.scroll_to_item(index);
                        }
                        inputs[next].1.focus(window);
                        cx.notify();
                        cx.stop_propagation();
                    }
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(glyph(Icon::Filter, 16., self.dark))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(t("Match")),
                    )
                    .child(
                        controls::segment_group(c).children(
                            [
                                (MatchMode::All, "filter-match-all", "Match all"),
                                (MatchMode::Any, "filter-match-any", "Match any"),
                            ]
                            .map(|(mode, id, label)| {
                                controls::segment(id, c, self.mode == mode)
                                    .child(t(label))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.mode = mode;
                                        this.error = None;
                                        cx.notify();
                                    }))
                            }),
                        ),
                    ),
            )
            .when(!self.whole_source, |editor| {
                editor.child(div().text_size(px(11.)).text_color(rgb(c.muted)).child(t(
                    "Filters apply to the loaded row window and selected fields.",
                )))
            })
            .child(if let Some(Picker::Operator(id)) = &self.picker {
                self.render_operator_picker(*id, cx)
            } else {
                div()
                    .id("filter-conditions")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.rows_scroll)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(self.rows.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_1()
                                .min_h(px(160.))
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .child(glyph(Icon::Filter, 24., self.dark))
                                .child(
                                    div()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .child(t("No conditions yet")),
                                )
                                .child(div().text_color(rgb(c.muted)).child(t(
                                    if self.whole_source {
                                        "Add a condition to choose which source rows to import."
                                    } else {
                                        "Add a condition to filter the loaded rows."
                                    },
                                ))),
                        )
                    })
                    .children(self.rows.iter().map(|row| self.render_row(row, window, cx)))
                    .into_any_element()
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(px(3.))
                        .bg(rgb(c.danger_bg))
                        .text_color(rgb(c.danger))
                        .whitespace_normal()
                        .child(t(error)),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_wrap()
                    .flex_shrink_0()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(c.border))
                    .child(self.button(
                        "filter-add",
                        "Add condition",
                        !self.targets.is_empty(),
                        false,
                        |this, window, cx| this.add_condition(window, cx),
                        cx,
                    ))
                    .child(self.button(
                        "filter-clear",
                        "Clear",
                        !self.rows.is_empty(),
                        false,
                        |this, window, cx| {
                            this.rows.clear();
                            this.calendar = None;
                            this.calendar_focus = None;
                            this.picker = None;
                            this.error = None;
                            this.focus.focus(window);
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(self.button(
                        "filter-cancel",
                        "Close",
                        true,
                        false,
                        |this, window, cx| this.hide(window, cx),
                        cx,
                    ))
                    .child(self.button(
                        "filter-apply",
                        "Apply",
                        true,
                        true,
                        |this, _, cx| this.apply(cx),
                        cx,
                    )),
            )
    }
}

fn quick_operators(kind: FilterFieldKind) -> &'static [(FilterOperator, &'static str)] {
    use FilterOperator::*;
    match kind {
        FilterFieldKind::Number => &[
            (Eq, "="),
            (NotEq, "≠"),
            (Gt, ">"),
            (Ge, "≥"),
            (Lt, "<"),
            (Le, "≤"),
            (Between, "Range"),
        ],
        FilterFieldKind::Date => &[
            (Eq, "On date"),
            (Lt, "Before"),
            (Gt, "After"),
            (Between, "Range"),
        ],
        FilterFieldKind::DateTime => &[
            (Eq, "At date/time"),
            (Lt, "Before"),
            (Gt, "After"),
            (Between, "Range"),
        ],
        FilterFieldKind::Time => &[
            (Eq, "At time"),
            (Lt, "Before"),
            (Gt, "After"),
            (Between, "Range"),
        ],
        FilterFieldKind::Text => &[
            (Eq, "="),
            (Contains, "Contains"),
            (StartsWith, "Starts with"),
            (EndsWith, "Ends with"),
        ],
        FilterFieldKind::Boolean => &[(Eq, "="), (NotEq, "≠")],
        FilterFieldKind::List => &[
            (Contains, "Contains value"),
            (NotContains, "Does not contain value"),
            (IsEmpty, "Empty List"),
            (IsNotEmpty, "Nonempty List"),
            (IsNull, "Is NULL"),
            (IsNotNull, "Is not NULL"),
        ],
        FilterFieldKind::NullOnly => &[(IsNull, "Is NULL"), (IsNotNull, "Is not NULL")],
    }
}

/// Calendar changes only the date. A timestamp's user-entered time, fractional
/// precision, separator and explicit zone remain byte-for-byte unchanged.
fn calendar_value(data_type: &str, previous: &str, date: chrono::NaiveDate) -> String {
    let date = date.format("%Y-%m-%d").to_string();
    if field_kind(data_type) != FilterFieldKind::DateTime {
        return date;
    }
    // The validator and calendar accept surrounding whitespace. Match them
    // here so choosing a day never discards a valid pasted time or UTC offset.
    let previous = previous.trim();
    if let Some(separator) = previous.find(['T', 't', ' '])
        && chrono::NaiveDate::parse_from_str(&previous[..separator], "%Y-%m-%d").is_ok()
    {
        return format!("{date}{}", &previous[separator..]);
    }
    let zone = if value_placeholder(data_type, false).contains("+HH:MM") {
        "+00:00"
    } else {
        ""
    };
    format!("{date} 00:00:00{zone}")
}

#[cfg(test)]
mod typed_ui_tests {
    use super::*;

    #[test]
    fn choosing_calendar_date_preserves_timestamp_time_zone_and_precision() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        for (data_type, previous, expected) in [
            ("DATE", "2024-02-29", "2026-10-31"),
            (
                "TIMESTAMP_NS",
                "2024-02-29T12:34:56.123456789",
                "2026-10-31T12:34:56.123456789",
            ),
            (
                "TIMESTAMPTZ",
                "2024-02-29 12:34:56.123456+08:00",
                "2026-10-31 12:34:56.123456+08:00",
            ),
            (
                "TIMESTAMPTZ",
                "2024-02-29T12:34:56Z",
                "2026-10-31T12:34:56Z",
            ),
            (
                "TIMESTAMPTZ",
                " 2024-02-29T12:34:56.123456+08:00 ",
                "2026-10-31T12:34:56.123456+08:00",
            ),
            (
                "TIMESTAMP_NS",
                "\t2024-02-29 12:34:56.123456789\n",
                "2026-10-31 12:34:56.123456789",
            ),
            ("TIMESTAMP", "", "2026-10-31 00:00:00"),
            ("TIMESTAMPTZ", "", "2026-10-31 00:00:00+00:00"),
        ] {
            assert_eq!(calendar_value(data_type, previous, date), expected);
        }
    }

    #[test]
    fn nested_targets_keep_typed_controls_and_identifier_boundaries() {
        let columns = vec![
            ColumnInfo { name: "profile".into(), data_type: "STRUCT(\"a.b\" DECIMAL(20,4), flags BOOLEAN[], moments TIMESTAMP_NS[], children STRUCT(x INTEGER)[])".into(), nullable: true },
            ColumnInfo { name: "profile.a.b".into(), data_type: "VARCHAR".into(), nullable: true },
        ];
        let entries: Vec<_> = targets(&columns)
            .into_iter()
            .map(CachedTarget::new)
            .collect();
        let target = |field: &str, path: &[&str]| {
            entries
                .iter()
                .find(|entry| {
                    entry.target.field == field
                        && entry
                            .target
                            .path
                            .iter()
                            .map(String::as_str)
                            .eq(path.iter().copied())
                })
                .unwrap()
        };
        let nested = target("profile", &["a.b"]);
        let literal = target("profile.a.b", &[]);
        assert_eq!(nested.value_kind, FilterFieldKind::Number);
        assert_eq!(literal.value_kind, FilterFieldKind::Text);
        assert_ne!(nested.target.label, literal.target.label);
        assert!(nested.search.contains("profile.a.b"));
        let flags = target("profile", &["flags"]);
        assert_eq!(flags.kind, FilterFieldKind::List);
        assert_eq!(flags.value_kind, FilterFieldKind::Boolean);
        assert!(operators(&flags.target.data_type).contains(&FilterOperator::Contains));
        let children = target("profile", &["children"]);
        assert_eq!(children.kind, FilterFieldKind::List);
        assert_eq!(children.value_kind, FilterFieldKind::NullOnly);
        assert!(!operators(&children.target.data_type).contains(&FilterOperator::Contains));
        assert!(operators(&children.target.data_type).contains(&FilterOperator::IsEmpty));
    }

    #[test]
    fn list_timestamp_calendar_uses_element_type_and_preserves_time_precision() {
        let column = ColumnInfo {
            name: "moments".into(),
            data_type: "TIMESTAMP_NS[]".into(),
            nullable: true,
        };
        let entry = targets(&[column])
            .into_iter()
            .map(CachedTarget::new)
            .next()
            .unwrap();
        assert_eq!(entry.kind, FilterFieldKind::List);
        assert_eq!(entry.value_kind, FilterFieldKind::DateTime);
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        assert_eq!(
            calendar_value(
                &entry.target.value_type,
                "2024-02-29T12:34:56.123456789",
                date
            ),
            "2026-10-31T12:34:56.123456789"
        );
    }

    #[test]
    fn quick_operators_are_valid_for_the_actual_field_type() {
        for data_type in [
            "BIGINT",
            "DOUBLE",
            "DECIMAL(38,18)",
            "DATE",
            "TIMESTAMP_NS",
            "TIMESTAMPTZ",
            "TIME",
            "BOOL",
            "VARCHAR",
            "BLOB",
            "STRUCT(a INTEGER)",
        ] {
            for (operator, _) in quick_operators(field_kind(data_type)) {
                assert!(
                    operators(data_type).contains(operator),
                    "{data_type} {operator:?}"
                );
            }
        }
    }
}
