//! Export the exact immutable result visible when the save panel opens.
use super::*;
use crate::aggregation::AggregateResult;

enum ExportSource {
    Details(Arc<QueryResult>),
    Summary(Arc<AggregateResult>),
}

impl ExportSource {
    fn is_current(&self, viewer: &Viewer, cx: &App) -> bool {
        match self {
            Self::Details(result) => viewer
                .result
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(result, current)),
            Self::Summary(result) => viewer
                .aggregation_result_for_export(cx)
                .is_some_and(|current| Arc::ptr_eq(result, &current)),
        }
    }

    fn write(
        self,
        sources: &[PathBuf],
        path: &std::path::Path,
        format: ExportFormat,
        cancel: &AtomicBool,
    ) -> anyhow::Result<()> {
        let (data, columns) = match &self {
            Self::Details(result) => (&result.data, &result.columns),
            Self::Summary(result) => (&result.data, &result.columns),
        };
        crate::export::export_snapshot(data, columns, &[], sources, path, format, cancel)
    }
}

impl Viewer {
    pub(super) fn render_export_formats(
        &self,
        summary: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = Colors::new(self.preferences.dark);
        let scope = if summary {
            let n = self
                .aggregation_result_for_export(cx)
                .map_or(0, |result| result.data.row_count());
            format!("Export {n} summary rows.")
        } else {
            let n = self.result.as_ref().map_or(0, |result| result.row_count());
            format!(
                "Export {n} visible rows, including the current field selection, filter and sort."
            )
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(t(scope))
            .child(
                div()
                    .text_color(rgb(colors.muted))
                    .child(t("Choose a format. The macOS save dialog will open.")),
            )
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [
                        ("export-csv", "CSV (.csv)", ExportFormat::Csv, "csv"),
                        ("export-json", "JSON (.json)", ExportFormat::Json, "json"),
                        ("export-xlsx", "Excel (.xlsx)", ExportFormat::Xlsx, "xlsx"),
                        (
                            "export-xls",
                            "Excel 97–2003 (.xls)",
                            ExportFormat::Xls,
                            "xls",
                        ),
                        (
                            "export-parquet",
                            "Parquet (.parquet)",
                            ExportFormat::Parquet,
                            "parquet",
                        ),
                    ]
                    .into_iter()
                    .map(|(id, label, format, extension)| {
                        self.button(
                            id,
                            label,
                            true,
                            move |this, _, cx| this.export_dialog(summary, format, extension, cx),
                            cx,
                        )
                        .w(px(230.))
                        .h(px(48.))
                        .justify_start()
                        .gap_3()
                        .px_4()
                    }),
                ),
            )
            .into_any_element()
    }

    fn export_dialog(
        &mut self,
        summary: bool,
        format: ExportFormat,
        extension: &str,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let source = if summary {
            self.aggregation_result_for_export(cx)
                .map(ExportSource::Summary)
        } else {
            self.result.clone().map(ExportSource::Details)
        };
        let (Some(source), Some(info)) = (source, &self.info) else {
            return;
        };
        let paths = info.paths.clone();
        let directory = paths
            .first()
            .and_then(|path| path.parent())
            .unwrap_or(std::path::Path::new("/tmp"));
        let name = if summary { "summary" } else { "export" };
        let response = cx.prompt_for_new_path(directory, Some(&format!("{name}.{extension}")));
        let prompted_generation = self.generation;
        cx.spawn(async move |weak, cx| {
            let Ok(Ok(Some(path))) = response.await else {
                cx.background_executor()
                    .spawn(async move { drop(source) })
                    .detach();
                return;
            };
            let cancel = Arc::new(AtomicBool::new(false));
            let operation = cancel.clone();
            let Ok(Some(generation)) = weak.update(cx, |this, cx| {
                if this.busy.is_some() {
                    return None;
                }
                if this.generation != prompted_generation || !source.is_current(this, cx) {
                    this.fail("The displayed result changed before export. Try again.", cx);
                    return None;
                }
                this.operation_cancel = Some(operation);
                this.generation = this.generation.wrapping_add(1);
                this.busy = Some(
                    if summary {
                        "Exporting summary results…"
                    } else {
                        "Exporting current results…"
                    }
                    .into(),
                );
                this.panel = None;
                this.error = None;
                cx.notify();
                Some(this.generation)
            }) else {
                cx.background_executor()
                    .spawn(async move { drop(source) })
                    .detach();
                return;
            };
            let target = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { source.write(&paths, &target, format, &cancel) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.busy = None;
                this.query_progress = None;
                this.operation_cancel = None;
                match result {
                    Ok(()) => this.notice = format!("Saved {}", path.display()),
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
