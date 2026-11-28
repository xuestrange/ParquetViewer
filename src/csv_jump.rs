//! Navigate by one-based source row without scanning an unindexed prefix on the UI thread.
use super::*;
use crate::engine::csv_source::CsvIndexProgress;
use std::num::NonZeroUsize;

#[derive(Debug, PartialEq, Eq)]
enum Destination {
    Show(usize),
    Waiting,
    Unavailable(&'static str),
}

fn destination(
    target: NonZeroUsize,
    order: &crate::table_state::RowOrder,
    index: &CsvIndexProgress,
) -> Destination {
    if let Some(display) = order.view(target.get() - 1) {
        Destination::Show(display)
    } else if index.error.is_some() {
        Destination::Unavailable("Indexing stopped before the requested row.")
    } else if index.complete {
        Destination::Unavailable("The requested row is beyond the end of the file.")
    } else {
        Destination::Waiting
    }
}

impl Viewer {
    pub(super) fn open_csv_jump(&mut self, cx: &mut Context<Self>) {
        if self.csv_browse.is_none() || self.panel.is_some() || self.busy.is_some() {
            return;
        }
        self.csv_jump.update(cx, |input, cx| input.set_text("", cx));
        self.show_panel(Panel::CsvJump, cx);
    }

    pub(super) fn pending_csv_jump_label(&self) -> Option<String> {
        let target = self.csv_browse.as_ref()?.pending_jump?;
        Some(t("Waiting for row {row}").replace("{row}", &count_label(target.get())))
    }

    pub(super) fn submit_csv_jump(&mut self, cx: &mut Context<Self>) {
        if self.panel != Some(Panel::CsvJump) || self.busy.is_some() {
            return;
        }
        let Ok(target) = self.csv_jump.read(cx).text().trim().parse::<NonZeroUsize>() else {
            return;
        };
        let (Some(browse), Some(index)) = (&mut self.csv_browse, &self.csv_index) else {
            return;
        };
        browse.pending_jump = Some(target);
        let index = index.progress();
        self.panel = None;
        self.error = None;
        self.notice.clear();
        self.sync_csv_browse_rows(index.indexed_rows, cx);
        self.resolve_csv_jump(&index, cx);
        cx.notify();
    }

    pub(super) fn resolve_csv_jump(&mut self, index: &CsvIndexProgress, cx: &mut Context<Self>) {
        let Some(browse) = &mut self.csv_browse else {
            return;
        };
        let Some(target) = browse.pending_jump else {
            return;
        };
        // Preserve modal focus and the old viewport during an operation. The next
        // index poll retries even if EOF was published while a dialog was open.
        if self.panel.is_some() || self.busy.is_some() {
            return;
        }
        match destination(target, &self.view_rows, index) {
            Destination::Waiting => return,
            Destination::Unavailable(message) => self.error = Some(message.into()),
            Destination::Show(row) => {
                let (pane, local) = if row < self.frozen_row_count {
                    (&self.pinned_scroll, row)
                } else {
                    (&self.scroll, row - self.frozen_row_count)
                };
                pane.main.scroll_to(gpui::ListOffset {
                    item_ix: local,
                    offset_in_item: px(0.),
                });
                pane.sync_from(false);
                let column = self.selection.map_or(0, |(_, end)| end.1);
                self.selection_kind = SelectionKind::Cells;
                self.selection = Some(((row, column), (row, column)));
                self.selecting = false;
            }
        }
        browse.pending_jump = None;
        cx.notify();
    }

    pub(super) fn render_csv_jump(&self, cx: &mut Context<Self>) -> gpui::Div {
        let c = Colors::new(self.preferences.dark);
        let input = self.csv_jump.read(cx).text();
        let enabled = input.trim().parse::<NonZeroUsize>().is_ok();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_color(rgb(c.muted)).whitespace_normal().child(t(
                "A row beyond the indexed range opens when indexing reaches it.",
            )))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(t("Source row number"))
                    .child(self.input_shell(&self.csv_jump, None).flex_1()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .when_some(self.pending_csv_jump_label(), |row, label| {
                        row.child(
                            div()
                                .flex_1()
                                .truncate()
                                .text_color(rgb(c.muted))
                                .child(label),
                        )
                        .child(self.button(
                            "csv-jump-cancel",
                            "Cancel pending jump",
                            true,
                            |this, window, cx| {
                                if let Some(browse) = &mut this.csv_browse {
                                    browse.pending_jump = None;
                                }
                                this.dismiss_panel(window, cx);
                            },
                            cx,
                        ))
                    })
                    .child(self.button(
                        "csv-jump-go",
                        "Go",
                        enabled,
                        |this, _, cx| this.submit_csv_jump(cx),
                        cx,
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jumps_use_exact_source_rows_and_wait_for_the_index() {
        let state = CsvIndexProgress {
            indexed_rows: 10,
            indexed_bytes: 20,
            total_bytes: 50,
            complete: false,
            error: None,
        };
        let order = crate::table_state::RowOrder::new(10, vec![7]);
        let row = |n| NonZeroUsize::new(n).unwrap();
        assert_eq!(destination(row(1), &order, &state), Destination::Show(1));
        assert_eq!(destination(row(8), &order, &state), Destination::Show(0));
        assert_eq!(destination(row(10), &order, &state), Destination::Show(9));
        assert_eq!(destination(row(11), &order, &state), Destination::Waiting);
        let grown = crate::table_state::RowOrder::new(11, vec![7]);
        assert_eq!(destination(row(11), &grown, &state), Destination::Show(10));
        let stopped = CsvIndexProgress {
            error: Some("error".into()),
            ..state.clone()
        };
        assert!(matches!(
            destination(row(11), &order, &stopped),
            Destination::Unavailable(_)
        ));
        assert_eq!(destination(row(10), &order, &stopped), Destination::Show(9));
        let complete = CsvIndexProgress {
            complete: true,
            ..state
        };
        assert!(matches!(
            destination(row(11), &order, &complete),
            Destination::Unavailable(_)
        ));
    }
}
