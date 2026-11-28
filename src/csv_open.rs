//! Transitions between a growing text file and a fixed analysis snapshot.
use super::*;
use crate::engine::SortKey;

pub(super) enum BrowseCommand {
    Export,
    Statistics,
    Filter,
    Sort(Vec<SortKey>, Option<usize>),
    SortOptions(Vec<String>),
}

impl Viewer {
    pub(super) fn install_csv_first_page(
        &mut self,
        index: crate::engine::csv_source::CsvBrowse,
        request: &QueryRequest,
        result: &QueryResult,
        cx: &mut Context<Self>,
    ) {
        self.csv_default_view = true;
        self.csv_first_page_rows = request.limit;
        self.count.update(cx, |input, cx| {
            input.set_text(&request.limit.to_string(), cx)
        });
        if !result.total_rows_known || result.row_count() < result.total_rows {
            self.csv_browse = Some(csv_browse::CsvBrowse::new(index.clone(), result));
        }
        self.csv_index = Some(index);
    }

    pub(super) fn stop_csv_browse(&mut self) {
        self.csv_browse = None;
        if let Some(index) = self.csv_index.take() {
            index.cancel();
        }
        self.csv_first_page_rows = 0;
        self.csv_default_view = false;
        self.csv_index_tick = None;
        self.browse_command = None;
    }

    pub(super) fn poll_csv_index(&mut self, cx: &mut Context<Self>) {
        let Some(index) = &self.csv_index else { return };
        let state = index.progress();
        let tick = (state.indexed_bytes, state.complete, state.error.is_some());
        if self.csv_index_tick == Some(tick) {
            self.resolve_csv_jump(&state, cx);
            return;
        }
        self.csv_index_tick = Some(tick);
        if let Some(info) = &mut self.info {
            info.row_count = state.indexed_rows;
            info.row_count_known = state.complete;
        }
        if self.csv_browse.is_some() {
            self.sync_csv_browse_rows(state.indexed_rows, cx);
            self.resolve_csv_jump(&state, cx);
        }
        cx.notify();
    }

    pub(super) fn resume_csv_browse(&mut self, cx: &mut Context<Self>) {
        let Some(info) = &self.info else { return };
        if !self.can_resume_csv_browse() || self.busy.is_some() || self.panel.is_some() {
            return;
        }
        let request = QueryRequest {
            selected_columns: info.columns.iter().map(|c| c.name.clone()).collect(),
            limit: self.csv_first_page_rows.max(1),
            ..Default::default()
        };
        self.execute_table_request(request, FilterGroup::default(), None, true, cx);
    }

    pub(super) fn can_resume_csv_browse(&self) -> bool {
        self.csv_browse.is_none()
            && self.csv_index.is_some()
            && self.info.as_ref().is_some_and(|info| {
                !info.row_count_known || info.row_count > self.csv_first_page_rows
            })
    }

    pub(super) fn suggest_browse_range(&self, cx: &mut Context<Self>) {
        let view = self
            .scroll
            .main
            .logical_scroll_top()
            .item_ix
            .saturating_add(self.frozen_row_count);
        let offset = self.view_rows.source(view).unwrap_or(0);
        self.offset
            .update(cx, |input, cx| input.set_text(&offset.to_string(), cx));
        self.count.update(cx, |input, cx| {
            input.set_text(&self.csv_first_page_rows.max(1).to_string(), cx)
        });
    }

    pub(super) fn choose_browse_range(&mut self, command: BrowseCommand, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.panel.is_some() || self.selecting_initial_fields {
            return;
        }
        self.browse_command = Some(command);
        self.axis_menu = None;
        self.selection_menu = None;
        self.show_panel(Panel::Range, cx);
    }

    pub(super) fn finish_browse_command(&mut self, cx: &mut Context<Self>) {
        let (Some(command), Some(handle)) = (self.browse_command.take(), self.main_window) else {
            return;
        };
        let generation = self.generation;
        cx.defer(move |cx| {
            let _ = handle.update(cx, |this, window, cx| {
                if this.generation != generation || this.busy.is_some() {
                    return;
                }
                match command {
                    BrowseCommand::Export => this.show_panel(Panel::Export, cx),
                    BrowseCommand::Statistics => this.toggle_statistics_sidebar(window, cx),
                    BrowseCommand::Filter => this.toggle_filter_sidebar(window, cx),
                    BrowseCommand::Sort(keys, limit) => this.execute_sort(keys, limit, cx),
                    BrowseCommand::SortOptions(fields) => this.open_sort_controls(fields, cx),
                }
            });
        });
    }
}
