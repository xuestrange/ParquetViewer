//! Find values and source positions in the current immutable result.
use super::*;

use crate::{
    filtering::{FilterFieldKind, field_kind},
    table_state::RowOrder,
};

pub(super) struct FindControls {
    value: Entity<TextInput>,
    row_number: Entity<TextInput>,
    row_focus: bool,
    match_case: bool,
    field: Option<String>,
    choosing_field: bool,
    pending: Option<Arc<AtomicBool>>,
    message: Option<&'static str>,
    _inputs: [Subscription; 2],
}

fn searchable(column: &ColumnInfo) -> bool {
    matches!(
        field_kind(&column.data_type),
        FilterFieldKind::Text
            | FilterFieldKind::Number
            | FilterFieldKind::Boolean
            | FilterFieldKind::Date
            | FilterFieldKind::DateTime
            | FilterFieldKind::Time
    )
}

/// Match literal text without allocating a lowercased copy of each cell.
/// Unicode lowercase expansions are streamed through a prefix matcher.
enum Needle {
    Literal(String),
    Lowercase {
        pattern: Vec<char>,
        prefix: Vec<usize>,
    },
}

impl Needle {
    fn new(value: String, match_case: bool) -> Self {
        if match_case {
            return Self::Literal(value);
        }
        let pattern: Vec<_> = value.chars().flat_map(char::to_lowercase).collect();
        let mut prefix = vec![0; pattern.len()];
        for index in 1..pattern.len() {
            let mut length = prefix[index - 1];
            while length > 0 && pattern[index] != pattern[length] {
                length = prefix[length - 1];
            }
            prefix[index] = length + usize::from(pattern[index] == pattern[length]);
        }
        Self::Lowercase { pattern, prefix }
    }

    fn matches(&self, text: &str, cancel: &AtomicBool) -> bool {
        match self {
            Self::Literal(value) => text.contains(value),
            Self::Lowercase { pattern, prefix } => {
                let mut length = 0;
                for (index, ch) in text.chars().flat_map(char::to_lowercase).enumerate() {
                    if index % 4096 == 0 && cancel.load(Ordering::Relaxed) {
                        return false;
                    }
                    while length > 0 && ch != pattern[length] {
                        length = prefix[length - 1];
                    }
                    length += usize::from(ch == pattern[length]);
                    if length == pattern.len() {
                        return true;
                    }
                }
                false
            }
        }
    }
}

struct ValueSearch {
    rows: RowOrder,
    columns: Vec<usize>,
    start: usize,
    backward: bool,
    needle: Needle,
}

impl ValueSearch {
    fn run(self, result: &QueryResult, cancel: &AtomicBool) -> Option<(usize, usize)> {
        let width = self.columns.len();
        let total = result.row_count().checked_mul(width)?;
        let mut position = self.start;
        for visited in 0..total {
            if visited % 256 == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let view_row = position / width;
            let row = self.rows.get(view_row).unwrap_or(view_row);
            let column = self.columns[position % width];
            if result
                .data
                .search_scalar_text(row, column)
                .is_some_and(|text| self.needle.matches(&text, cancel))
            {
                return Some((row, column));
            }
            position = step(position, total, self.backward);
        }
        None
    }
}

fn step(position: usize, total: usize, backward: bool) -> usize {
    if backward {
        position.checked_sub(1).unwrap_or(total - 1)
    } else if position + 1 == total {
        0
    } else {
        position + 1
    }
}

fn find_source_row(
    result: &QueryResult,
    target: usize,
    first: usize,
    sorted: bool,
    cancel: &AtomicBool,
) -> Option<usize> {
    // On an unfiltered range the original position is usually a direct hit.
    if let Some(index) = target.checked_sub(first)
        && result.source_row_number(index) == Some(target)
    {
        return Some(index);
    }
    if !sorted {
        // The ordinary filter path preserves the source order.
        let mut start = 0;
        let mut end = result.row_count();
        while start < end {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let middle = start + (end - start) / 2;
            match result.source_row_number(middle)?.cmp(&target) {
                std::cmp::Ordering::Less => start = middle + 1,
                std::cmp::Ordering::Greater => end = middle,
                std::cmp::Ordering::Equal => return Some(middle),
            }
        }
        return None;
    }
    // Sorting can place the original row anywhere in the displayed result.
    for row in 0..result.row_count() {
        if row % 1024 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        if result.source_row_number(row) == Some(target) {
            return Some(row);
        }
    }
    None
}

impl Viewer {
    pub(super) fn open_find(&mut self, row_focus: bool, cx: &mut Context<Self>) {
        if self.busy.is_some()
            || self.panel.is_some_and(|panel| panel != Panel::Find)
            || self.result.as_ref().is_none_or(|result| result.is_empty())
        {
            return;
        }
        if self.find.is_none() {
            let dark = self.preferences.dark;
            let value = cx.new(|cx| {
                TextInput::new(cx)
                    .with_dark(dark)
                    .with_placeholder(&t("Value to find"))
            });
            let row_number = cx.new(|cx| {
                TextInput::new(cx)
                    .with_dark(dark)
                    .with_placeholder(&t("Original row number"))
            });
            let value_subscription = cx.subscribe(&value, |this, _, event, cx| match event {
                InputEvent::Changed => this.find_input_changed(cx),
                InputEvent::Submit => this.find_value(false, cx),
            });
            let row_subscription = cx.subscribe(&row_number, |this, _, event, cx| match event {
                InputEvent::Changed => this.find_input_changed(cx),
                InputEvent::Submit => this.jump_to_source_row(cx),
            });
            self.find = Some(FindControls {
                value,
                row_number,
                row_focus,
                match_case: false,
                field: None,
                choosing_field: false,
                pending: None,
                message: None,
                _inputs: [value_subscription, row_subscription],
            });
        }
        self.cancel_find();
        if let Some(find) = &mut self.find {
            find.row_focus = row_focus;
            find.choosing_field = false;
            find.message = None;
        }
        self.settings_menu = None;
        self.axis_menu = None;
        self.selection_menu = None;
        self.error = None;
        self.panel = Some(Panel::Find);
        self.focused_panel = None;
        cx.notify();
    }

    pub(super) fn find_focus(&self, cx: &App) -> FocusHandle {
        self.find.as_ref().map_or_else(
            || self.focus.clone(),
            |find| {
                if find.row_focus {
                    &find.row_number
                } else {
                    &find.value
                }
                .focus_handle(cx)
            },
        )
    }

    pub(super) fn find_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.find.as_ref().is_some_and(|find| {
            find.value.focus_handle(cx).is_focused(window)
                || find.row_number.focus_handle(cx).is_focused(window)
        })
    }

    pub(super) fn sync_find_inputs(&mut self, cx: &mut Context<Self>) {
        if let Some(find) = &self.find {
            let dark = self.preferences.dark;
            for (input, placeholder) in [
                (&find.value, "Value to find"),
                (&find.row_number, "Original row number"),
            ] {
                input.update(cx, |input, cx| {
                    input.set_dark(dark, cx);
                    input.set_placeholder(&t(placeholder), cx);
                });
            }
        }
    }

    pub(super) fn cancel_find(&mut self) {
        if let Some(find) = &mut self.find
            && let Some(cancel) = find.pending.take()
        {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn reset_find(&mut self) {
        self.cancel_find();
        self.find = None;
    }

    fn find_input_changed(&mut self, cx: &mut Context<Self>) {
        self.cancel_find();
        if let Some(find) = &mut self.find {
            find.message = None;
        }
        cx.notify();
    }

    fn find_message(&mut self, message: &'static str, cx: &mut Context<Self>) {
        if self.panel == Some(Panel::Find) {
            if let Some(find) = &mut self.find {
                find.message = Some(message);
            }
        } else {
            self.notice = message.into();
        }
        cx.notify();
    }

    pub(super) fn find_value(&mut self, backward: bool, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.panel.is_some_and(|panel| panel != Panel::Find) {
            return;
        }
        let Some(find) = &self.find else {
            self.open_find(false, cx);
            return;
        };
        let Some(result) = self.result.clone().filter(|result| !result.is_empty()) else {
            return;
        };
        let value = find.value.read(cx).text();
        if value.is_empty() {
            if self.panel != Some(Panel::Find) {
                self.open_find(false, cx);
            }
            self.find_message("Enter a value to find.", cx);
            return;
        }
        let columns: Vec<_> = self
            .view_columns
            .iter()
            .copied()
            .filter(|&column| {
                let column = &result.columns[column];
                searchable(column) && find.field.as_ref().is_none_or(|name| name == &column.name)
            })
            .collect();
        let Some(total) = result
            .row_count()
            .checked_mul(columns.len())
            .filter(|n| *n > 0)
        else {
            self.find_message("Choose an available scalar field.", cx);
            return;
        };
        let width = columns.len();
        let start = self.selection.map_or_else(
            || if backward { total - 1 } else { 0 },
            |(_, (row, column))| {
                let row = row.min(result.row_count() - 1);
                columns
                    .iter()
                    .position(|&c| c == self.source_column(column))
                    .map_or_else(
                        || row * width + if backward { width - 1 } else { 0 },
                        |column| step(row * width + column, total, backward),
                    )
            },
        );
        let rows = self.view_rows.clone();
        let match_case = find.match_case;
        self.start_find_job(
            result,
            move |result, cancel| {
                ValueSearch {
                    rows,
                    columns,
                    start,
                    backward,
                    needle: Needle::new(value, match_case),
                }
                .run(result, cancel)
            },
            "No matching value in the current result.",
            cx,
        );
    }

    fn jump_to_source_row(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.panel != Some(Panel::Find) {
            return;
        }
        let (Some(find), Some(result)) = (&self.find, self.result.clone()) else {
            return;
        };
        let number = find.row_number.read(cx).text();
        let Ok(target) = number.trim().parse::<usize>() else {
            self.find_message("Enter a positive original row number.", cx);
            return;
        };
        if target == 0 {
            self.find_message("Enter a positive original row number.", cx);
            return;
        }
        let (offset, end) = if self.displayed_request.whole_source {
            (0, result.total_rows_known.then_some(result.total_rows))
        } else {
            let offset = self.displayed_request.offset;
            (offset, Some(offset.saturating_add(result.loaded_rows)))
        };
        if target <= offset || end.is_some_and(|end| target > end) {
            self.find_message("That original row is not in the current result.", cx);
            return;
        }
        let sorted = self.displayed_request.sort_rules().next().is_some();
        let column = self.source_column(self.selection.map_or(0, |(_, (_, column))| column));
        self.start_find_job(
            result,
            move |result, cancel| {
                find_source_row(result, target, offset + 1, sorted, cancel).map(|row| (row, column))
            },
            "That original row is not in the current result.",
            cx,
        );
    }

    fn start_find_job(
        &mut self,
        result: Arc<QueryResult>,
        work: impl FnOnce(&QueryResult, &AtomicBool) -> Option<(usize, usize)> + Send + 'static,
        miss: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(window_handle) = self.main_window else {
            return;
        };
        self.cancel_find();
        let Some(find) = &mut self.find else { return };
        let cancel = Arc::new(AtomicBool::new(false));
        find.pending = Some(cancel.clone());
        find.choosing_field = false;
        find.message = None;
        let expected = Arc::downgrade(&result);
        let token = cancel.clone();
        let task = cx
            .background_executor()
            .spawn(async move { work(&result, &cancel) });
        cx.spawn(async move |_, cx| {
            let location = task.await;
            let _ = window_handle.update(cx, |this, window, cx| {
                let Some(find) = &mut this.find else { return };
                if find
                    .pending
                    .as_ref()
                    .is_none_or(|current| !Arc::ptr_eq(current, &token))
                {
                    return;
                }
                find.pending = None;
                if this.result.as_ref().is_none_or(|current| {
                    !std::sync::Weak::ptr_eq(&Arc::downgrade(current), &expected)
                }) {
                    cx.notify();
                    return;
                }
                let Some((row, column)) = location else {
                    this.find_message(miss, cx);
                    return;
                };
                let (Some(view_row), Some(view_column)) =
                    (this.view_row(row), this.view_column(column))
                else {
                    return;
                };
                if this.panel == Some(Panel::Find) {
                    this.panel = None;
                }
                this.settings_menu = None;
                this.axis_menu = None;
                this.selection_menu = None;
                this.selection_kind = SelectionKind::Cells;
                this.selecting = false;
                this.selection = Some(((view_row, view_column), (view_row, view_column)));
                this.reveal_row(view_row);
                this.reveal_column(view_column, window);
                if let Some(number) = this
                    .result
                    .as_ref()
                    .and_then(|result| result.source_row_number(row))
                {
                    this.notice = format!("Original row number {number}");
                }
                this.focus.focus(window);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_find_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(find) = &self.find else {
            return div().into_any_element();
        };
        let c = Colors::new(self.preferences.dark);
        let pending = find.pending.is_some();
        let enabled = self.busy.is_none() && !pending;
        let scope_label = find.field.clone().unwrap_or_else(|| t("All scalar fields"));
        let fields = self.result.iter().flat_map(|result| {
            result
                .columns
                .iter()
                .filter(|column| searchable(column))
                .map(|column| column.name.clone())
        });
        div()
            .flex().flex_col().gap_3().min_h_0().flex_1()
            .child(div().text_color(rgb(c.muted)).child(t("Find literal text in the current result. Text values are searched in full. Other scalar values use their standard representation.")))
            .child(self.input_shell(&find.value, None).flex_none())
            .child(div().flex().items_center().gap_2()
                .child(div().text_color(rgb(c.muted)).child(t("Search in")))
                .child(controls::selector("find-field-scope", c, find.choosing_field, enabled)
                    .flex_1().justify_between()
                    .child(div().min_w_0().truncate().child(scope_label))
                    .child(controls::chevron(c, find.choosing_field))
                    .when(enabled, |el| el.on_click(cx.listener(|this, _, _, cx| {
                            if let Some(find) = &mut this.find {
                                find.choosing_field = !find.choosing_field;
                            }
                            cx.notify();
                        }))))
                .child(controls::checkbox("find-match-case", c, self.preferences.dark, find.match_case, enabled)
                    .child(t("Match case"))
                    .when(enabled, |el| el.on_click(cx.listener(|this, _, window, cx| {
                        if let Some(find) = &mut this.find {
                            find.match_case = !find.match_case;
                        }
                        this.focus.focus(window);
                        this.find_input_changed(cx);
                    })))))
            .when(find.choosing_field, |body| body.child(
                div().id("find-fields").max_h(px(116.)).min_h_0().overflow_y_scroll()
                    .border_1().border_color(rgb(c.border)).rounded(px(3.))
                    .children(std::iter::once(None).chain(fields.map(Some)).enumerate().map(|(index, field)| {
                        let label = field.clone().unwrap_or_else(|| t("All scalar fields"));
                        controls::menu_item(("find-field", index), c, find.field == field, true)
                            .h(px(26.))
                            .child(div().min_w_0().truncate().child(label))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(find) = &mut this.find {
                                    find.field = field.clone();
                                    find.choosing_field = false;
                                }
                                this.find_input_changed(cx);
                            }))
                    }))
            ))
            .child(div().flex().gap_2().items_center()
                .child(self.button("find-previous", "Find Previous", enabled, |this, _, cx| this.find_value(true, cx), cx))
                .child(self.button("find-next", "Find Next", enabled, |this, _, cx| this.find_value(false, cx), cx))
                .when(pending, |el| el.child(self.button("find-cancel", "Cancel", true, |this, _, cx| {
                    this.cancel_find();
                    cx.notify();
                }, cx)))
                .when(pending, |el| el.child(t("Searching current result…"))))
            .child(div().h(px(1.)).bg(rgb(c.border)))
            .child(div().text_color(rgb(c.muted)).child(t("Go to a 1-based original row in the current result.")))
            .child(div().flex().gap_2().items_center()
                .child(self.input_shell(&find.row_number, None))
                .child(self.button("go-to-original-row", "Go to Row", enabled, |this, _, cx| this.jump_to_source_row(cx), cx)))
            .when_some(find.message, |el, message| el.child(div().text_color(rgb(c.warning)).child(t(message))))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_literal_search_handles_expansions_and_prefix_overlap() {
        let cancel = AtomicBool::new(false);
        assert!(Needle::new("i\u{307}abc".into(), false).matches("İABC", &cancel));
        assert!(Needle::new("aaab".into(), false).matches("aaaaAB", &cancel));
        assert!(!Needle::new("ABC".into(), true).matches("abc", &cancel));
    }

    #[test]
    fn value_search_uses_full_arrow_text_and_frozen_display_order() {
        let column = ColumnInfo {
            name: "notes".into(),
            data_type: "VARCHAR".into(),
            nullable: true,
        };
        let result = QueryResult::from_cells(
            vec![column],
            vec![
                vec![crate::engine::Cell {
                    text: format!("{}NEEDLE", "x".repeat(3000)),
                    ..Default::default()
                }],
                vec![crate::engine::Cell {
                    text: "NULL".into(),
                    is_null: true,
                    ..Default::default()
                }],
                vec![crate::engine::Cell {
                    text: "Needle in a frozen row".into(),
                    ..Default::default()
                }],
            ],
        );
        let run = |start, backward, value: &str| {
            ValueSearch {
                rows: RowOrder::new(3, vec![2]),
                columns: vec![0],
                start,
                backward,
                needle: Needle::new(value.to_owned(), false),
            }
            .run(&result, &AtomicBool::new(false))
        };
        assert_eq!(run(0, false, "needle"), Some((2, 0)));
        assert_eq!(run(1, false, "needle"), Some((0, 0)));
        assert_eq!(run(2, false, "needle"), Some((2, 0)));
        assert_eq!(run(2, true, "needle"), Some((0, 0)));
        assert_eq!(run(0, false, "NULL"), None);
    }

    #[test]
    fn source_jump_keeps_nonzero_offset_and_distinguishes_filtered_out_rows() -> anyhow::Result<()>
    {
        let mut result = QueryResult::from_cells(
            vec![ColumnInfo {
                name: "id".into(),
                data_type: "BIGINT".into(),
                nullable: false,
            }],
            (0..4)
                .map(|row| {
                    vec![crate::engine::Cell {
                        text: row.to_string(),
                        ..Default::default()
                    }]
                })
                .collect(),
        );
        let base = result.data.with_source_row_start(100);
        let cancel = AtomicBool::new(false);
        result.data = base.with_row_indices(arrow::array::UInt64Array::from(vec![0, 2, 3]))?;
        assert_eq!(find_source_row(&result, 102, 100, false, &cancel), Some(1));
        assert_eq!(find_source_row(&result, 101, 100, false, &cancel), None);
        result.data = base.with_row_indices(arrow::array::UInt64Array::from(vec![3, 0, 2]))?;
        assert_eq!(find_source_row(&result, 100, 100, true, &cancel), Some(1));
        assert_eq!(find_source_row(&result, 103, 100, true, &cancel), Some(0));
        Ok(())
    }
}
