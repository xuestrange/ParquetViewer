//! Snapshot-bound, asynchronous aggregation. Detail rows remain untouched.
use super::*;
use crate::{aggregation::AggregateResult, aggregation_charts::AggregateCharts};

#[path = "aggregation_drilldown.rs"]
mod drilldown;

pub(super) struct AggregationDetail {
    base: Arc<QueryResult>,
    rows: TableState,
}

impl Viewer {
    pub(super) fn dismiss_aggregation_picker(&self, cx: &mut Context<Self>) {
        if let Some(editor) = &self.aggregation_editor {
            editor.update(cx, |editor, cx| editor.dismiss_picker(cx));
        }
    }

    pub(super) fn reset_aggregation_controls(&mut self, cx: &mut Context<Self>) {
        self.cancel_aggregation_chart_refresh();
        self.close_statistics_sidebar(cx);
        self.close_column_profile();
        self.restore_aggregation_details(cx);
        self.aggregation_subscription = None;
        self.aggregation_observer = None;
        if let Some(editor) = self.aggregation_editor.take() {
            editor.update(cx, |editor, cx| editor.clear_result(cx));
        }
        self.statistics_scroll = ScrollHandle::new();
        self.profile_field_scroll = gpui::UniformListScrollHandle::new();
        self.aggregation_scroll = ScrollHandle::new();
    }

    pub(super) fn reconcile_aggregation_result(&mut self, cx: &mut Context<Self>) {
        self.cancel_aggregation_chart_refresh();
        if let Some(previous) = self.aggregation_base.take() {
            cx.background_executor()
                .spawn(async move { drop(previous) })
                .detach();
        }
        let Some(editor) = &self.aggregation_editor else {
            return;
        };
        let columns = self
            .result
            .as_ref()
            .map_or_else(Vec::new, |result| result.columns.clone());
        editor.update(cx, |editor, cx| {
            editor.reconcile_columns(columns, cx);
            editor.clear_result(cx);
            editor.set_detail_view(false, None, cx);
        });
    }

    pub(super) fn aggregation_result_for_export(&self, cx: &App) -> Option<Arc<AggregateResult>> {
        if self.statistics_tab != Some(StatisticsTab::Aggregation) {
            return None;
        }
        self.aggregation_editor
            .as_ref()?
            .read(cx)
            .result_for_export()
    }

    pub(super) fn restore_aggregation_details(&mut self, cx: &mut Context<Self>) {
        let Some(mut previous) = self.aggregation_base.take() else {
            return;
        };
        if self.aggregation_inflight {
            self.cancel(cx);
        }
        self.table_state.swap_rows(&mut previous.rows);
        self.publish_aggregation_details(previous.base, cx);
        if let Some(editor) = &self.aggregation_editor {
            editor.update(cx, |editor, cx| editor.set_detail_view(false, None, cx));
        }
        cx.background_executor()
            .spawn(async move { drop(previous.rows) })
            .detach();
    }

    fn publish_aggregation_details(&mut self, result: Arc<QueryResult>, cx: &mut Context<Self>) {
        self.close_preview(cx);
        self.audio = None;
        self.audio_cell = None;
        self.reset_table_scroll();
        self.replace_loaded_result(Some(result), cx);
        self.refresh_table_order(cx);
    }

    pub(super) fn prepare_aggregation_editor(&mut self, cx: &mut Context<Self>) {
        if self.aggregation_editor.is_none() {
            let Some(result) = &self.result else { return };
            let columns = result.columns.clone();
            let dark = self.preferences.dark;
            let focus = self.focus.clone();
            let editor = cx.new(|cx| {
                let mut editor = AggregationEditor::new(columns, dark, cx);
                editor.set_host_focus(focus);
                editor
            });
            self.aggregation_subscription =
                Some(cx.subscribe(&editor, |this, _, event, cx| match event {
                    AggregationEvent::Run => this.run_aggregation(cx),
                    AggregationEvent::Cancel => this.cancel(cx),
                    AggregationEvent::Close => this.close_statistics_sidebar(cx),
                    AggregationEvent::Export => this.show_panel(Panel::AggregateExport, cx),
                    AggregationEvent::DrillDown(row) => this.show_aggregation_details(*row, cx),
                    AggregationEvent::RestoreDetails => this.restore_aggregation_details(cx),
                }));
            self.aggregation_observer = Some(cx.observe(&editor, |_, _, cx| cx.notify()));
            self.aggregation_editor = Some(editor);
        }
    }

    fn run_aggregation(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.statistics_tab != Some(StatisticsTab::Aggregation) {
            return;
        }
        let (Some(engine), Some(snapshot), Some(editor)) = (
            self.engine.clone(),
            self.result
                .as_ref()
                .map(|result| result.snapshot_id.clone()),
            self.aggregation_editor.clone(),
        ) else {
            return;
        };
        let request = match editor.read(cx).request() {
            Ok(request) => request,
            Err(error) => {
                editor.update(cx, |editor, cx| editor.set_error(error.to_string(), cx));
                return;
            }
        };
        self.cancel_aggregation_chart_refresh();
        self.restore_aggregation_details(cx);
        let plot_limits = self.aggregation_plot_limits();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation_cancel = Some(cancel.clone());
        self.aggregation_inflight = true;
        self.busy = Some("Aggregating…".into());
        let progress = QueryProgress::default();
        progress.begin_phase(QueryPhase::Aggregating);
        self.query_progress = Some(progress.clone());
        editor.update(cx, |editor, cx| {
            editor.clear_result(cx);
            editor.set_busy(true, cx);
        });
        let snapshot_for_task = snapshot.clone();
        let task = cx.background_executor().spawn(async move {
            let result = engine
                .lock()
                .map_err(|_| anyhow::anyhow!("Data engine lock was poisoned"))?
                .aggregate_snapshot_with_progress(
                    &snapshot_for_task,
                    &request,
                    &cancel,
                    &progress,
                )?;
            progress.begin_phase(QueryPhase::Charting);
            let charts = AggregateCharts::new(
                &result,
                &request.metrics,
                plot_limits.0,
                plot_limits.1,
                &cancel,
            );
            Ok::<_, anyhow::Error>((result, request.metrics, charts))
        });
        cx.spawn(async move |weak, cx| {
            let mut outcome = Some(task.await);
            let _ = weak.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.busy = None;
                this.query_progress = None;
                this.operation_cancel = None;
                this.aggregation_inflight = false;
                editor.update(cx, |editor, cx| editor.set_busy(false, cx));
                if this.statistics_tab != Some(StatisticsTab::Aggregation)
                    || this
                        .result
                        .as_ref()
                        .is_none_or(|result| result.snapshot_id != snapshot)
                {
                    return;
                }
                match outcome.take().unwrap() {
                    Ok((result, metrics, charts)) => {
                        editor.update(cx, |editor, cx| {
                            editor.set_result(Arc::new(result), metrics.into(), charts, cx)
                        });
                        if this.aggregation_plot_limits() != plot_limits {
                            this.refresh_aggregation_charts(cx);
                        }
                    }
                    Err(error) => {
                        editor.update(cx, |editor, cx| editor.set_error(error.to_string(), cx))
                    }
                }
                cx.notify();
            });
            if let Some(stale) = outcome {
                cx.background_executor()
                    .spawn(async move {
                        drop(stale);
                    })
                    .detach();
            }
        })
        .detach();
        cx.notify();
    }

    fn aggregation_plot_limits(&self) -> (usize, usize) {
        (
            self.preferences.profile_chart_max_rows,
            self.preferences.profile_chart_category_bars,
        )
    }

    pub(super) fn cancel_aggregation_chart_refresh(&mut self) {
        if let Some(cancel) = self.aggregation_chart_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn refresh_aggregation_charts(&mut self, cx: &mut Context<Self>) {
        self.cancel_aggregation_chart_refresh();
        let Some(editor) = &self.aggregation_editor else {
            return;
        };
        let Some((revision, result, metrics)) = editor.read(cx).chart_input() else {
            return;
        };
        let limits = self.aggregation_plot_limits();
        let result = Arc::downgrade(&result);
        let cancel = Arc::new(AtomicBool::new(false));
        self.aggregation_chart_cancel = Some(cancel.clone());
        let task_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            if task_cancel.load(Ordering::Relaxed) {
                return None;
            }
            let result = result.upgrade()?;
            AggregateCharts::new(&result, &metrics, limits.0, limits.1, &task_cancel)
        });
        cx.spawn(async move |weak, cx| {
            let charts = task.await;
            let _ = weak.update(cx, |this, cx| {
                if !this
                    .aggregation_chart_cancel
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &cancel))
                {
                    return;
                }
                this.aggregation_chart_cancel = None;
                if let Some(editor) = &this.aggregation_editor {
                    editor.update(cx, |editor, cx| editor.set_charts(revision, charts, cx));
                }
            });
        })
        .detach();
    }

    fn show_aggregation_details(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.busy.is_some()
            || self.statistics_tab != Some(StatisticsTab::Aggregation)
            || self.panel.is_some()
        {
            return;
        }
        let (Some(view), Some(editor)) = (self.result.as_ref(), self.aggregation_editor.clone())
        else {
            return;
        };
        let Some((summary, group_count)) = editor.read(cx).drilldown_input(row) else {
            return;
        };
        let visible = Arc::as_ptr(view) as usize;
        let base = self
            .aggregation_base
            .as_ref()
            .map_or(view, |detail| &detail.base)
            .clone();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation_cancel = Some(cancel.clone());
        self.aggregation_inflight = true;
        self.busy = Some("Finding group details…".into());
        self.query_progress = None;
        editor.update(cx, |editor, cx| editor.set_drilldown_busy(cx));
        let task = cx.background_executor().spawn(async move {
            let data = drilldown::select_group(&base.data, &summary, group_count, row, &cancel)?;
            let mut result = base.as_ref().clone();
            result.data = data;
            Ok::<_, anyhow::Error>(Arc::new(result))
        });
        cx.spawn(async move |weak, cx| {
            let mut outcome = Some(task.await);
            let _ = weak.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.busy = None;
                this.operation_cancel = None;
                this.aggregation_inflight = false;
                editor.update(cx, |editor, cx| editor.set_busy(false, cx));
                if this.statistics_tab != Some(StatisticsTab::Aggregation)
                    || this
                        .result
                        .as_ref()
                        .is_none_or(|current| Arc::as_ptr(current) as usize != visible)
                {
                    return;
                }
                match outcome.take().unwrap() {
                    Ok(result) => {
                        if this.aggregation_base.is_none() {
                            let mut rows = TableState::default();
                            this.table_state.swap_rows(&mut rows);
                            this.aggregation_base = Some(AggregationDetail {
                                base: this.result.as_ref().unwrap().clone(),
                                rows,
                            });
                        } else {
                            this.table_state.clear_rows();
                        }
                        this.publish_aggregation_details(result, cx);
                        editor.update(cx, |editor, cx| editor.set_detail_view(true, Some(row), cx));
                    }
                    Err(error) => {
                        editor.update(cx, |editor, cx| editor.set_error(error.to_string(), cx))
                    }
                }
                cx.notify();
            });
            if let Some(stale) = outcome {
                cx.background_executor()
                    .spawn(async move { drop(stale) })
                    .detach();
            }
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_aggregation_section(
        &self,
        viewport_width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.aggregation_editor.as_ref().map_or_else(
            || div().into_any_element(),
            |editor| {
                editor.update(cx, |editor, _| editor.set_viewport_width(viewport_width));
                editor.clone().into_any_element()
            },
        )
    }
}
