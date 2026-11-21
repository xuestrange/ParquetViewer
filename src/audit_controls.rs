//! Dialogs for current-result key audits and whole-file version comparison.
use super::*;
use gpui::uniform_list;

use crate::data_audit::{
    ChangeKind, DuplicateRows, VersionDiff, VersionSources, duplicate_keys, supports_key,
};

type CompareProgress = Arc<Mutex<Option<(usize, usize)>>>;

pub(super) struct AuditControls {
    job: Option<Arc<AtomicBool>>,
    keys: Vec<String>,
    result_scroll: gpui::UniformListScrollHandle,
    key_scroll: gpui::UniformListScrollHandle,
    search: Entity<TextInput>,
    _search_subscription: Subscription,
    kind: AuditKind,
}

enum AuditKind {
    Duplicates {
        source: Arc<QueryResult>,
        result: Option<DuplicateRows>,
    },
    Compare {
        other: PathBuf,
        sources: Option<Arc<VersionSources>>,
        result: Option<VersionDiff>,
        show_schema: bool,
        progress: CompareProgress,
    },
}

impl AuditControls {
    fn new(kind: AuditKind, dark: bool, cx: &mut Context<Viewer>) -> Self {
        let search = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(dark)
                .with_placeholder(&t("Search fields"))
        });
        let subscription = cx.subscribe(&search, |this, _, _, cx| {
            if let Some(audit) = &mut this.audit {
                audit.key_scroll = gpui::UniformListScrollHandle::new();
                cx.notify();
            }
        });
        Self {
            job: None,
            keys: Vec::new(),
            result_scroll: gpui::UniformListScrollHandle::new(),
            key_scroll: gpui::UniformListScrollHandle::new(),
            search,
            _search_subscription: subscription,
            kind,
        }
    }

    fn key_choices(&self, cx: &App) -> Vec<&str> {
        let search = self.search.read(cx).text().trim().to_lowercase();
        let matching = |name: &str| search.is_empty() || name.to_lowercase().contains(&search);
        match &self.kind {
            AuditKind::Duplicates { source, .. } => source
                .columns
                .iter()
                .filter(|column| supports_key(&column.data_type) && matching(&column.name))
                .map(|column| column.name.as_str())
                .collect(),
            AuditKind::Compare { sources, .. } => {
                sources.as_ref().map_or_else(Vec::new, |sources| {
                    sources
                        .keys
                        .iter()
                        .map(String::as_str)
                        .filter(|name| matching(name))
                        .collect()
                })
            }
        }
    }

    fn owns_job(&self, job: &Arc<AtomicBool>) -> bool {
        self.job
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, job))
    }
}

impl Viewer {
    pub(super) fn audit_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.audit
            .as_ref()
            .is_some_and(|audit| audit.search.focus_handle(cx).is_focused(window))
    }

    pub(super) fn sync_audit_input(&mut self, cx: &mut Context<Self>) {
        if let Some(audit) = &self.audit {
            audit.search.update(cx, |input, cx| {
                input.set_dark(self.preferences.dark, cx);
                input.set_placeholder(&t("Search fields"), cx);
            });
        }
    }

    fn show_audit(&mut self, audit: AuditControls, cx: &mut Context<Self>) {
        self.error = None;
        self.selection_menu = None;
        self.settings_menu = None;
        self.axis_menu = None;
        self.panel = Some(Panel::DataAudit);
        self.audit = Some(audit);
        cx.notify();
    }

    pub(super) fn close_data_audit(&mut self, cx: &App) {
        if let Some(audit) = self.audit.take() {
            if let Some(job) = audit.job {
                job.store(true, Ordering::Release);
            }
            let kind = audit.kind;
            cx.background_executor()
                .spawn(async move { drop(kind) })
                .detach();
        }
        if self.panel == Some(Panel::DataAudit) {
            self.panel = None;
        }
    }

    pub(super) fn open_duplicate_audit(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.panel.is_some() {
            return;
        }
        if let Some(source) = self.result.clone() {
            let audit = AuditControls::new(
                AuditKind::Duplicates {
                    source,
                    result: None,
                },
                self.preferences.dark,
                cx,
            );
            self.show_audit(audit, cx);
        }
    }

    pub(super) fn open_version_comparison(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.panel.is_some() {
            return;
        }
        let Some(info) = &self.info else { return };
        let [original] = info.paths.as_slice() else {
            self.fail("Open a single file before comparing versions.", cx);
            return;
        };
        let original = original.clone();
        let csv_options = self.csv_options.clone();
        let response = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("Choose a file version to compare").into()),
        });
        cx.spawn(async move |weak, cx| {
            let answer = response.await;
            let _ = weak.update(cx, |this, cx| {
                if this.busy.is_some()
                    || this.panel.is_some()
                    || this
                        .info
                        .as_ref()
                        .is_none_or(|info| info.paths != [original.clone()])
                    || this.csv_options != csv_options
                {
                    return;
                }
                match answer {
                    Ok(Ok(Some(paths))) if paths.len() == 1 => {
                        let other = paths.into_iter().next().unwrap();
                        this.read_comparison_schema(original, other, cx);
                    }
                    Ok(Err(error)) => this.fail(error, cx),
                    _ => {}
                }
            });
        })
        .detach();
    }

    fn read_comparison_schema(
        &mut self,
        original: PathBuf,
        other: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let cancel = Arc::new(AtomicBool::new(false));
        let csv_options = self.csv_options.clone();
        let mut audit = AuditControls::new(
            AuditKind::Compare {
                other: other.clone(),
                sources: None,
                result: None,
                show_schema: true,
                progress: Arc::default(),
            },
            self.preferences.dark,
            cx,
        );
        audit.job = Some(cancel.clone());
        self.show_audit(audit, cx);
        let job = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            VersionSources::open_with_options(original, other, csv_options, &cancel).map(Arc::new)
        });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                let Some(audit) = this.audit.as_mut().filter(|audit| audit.owns_job(&job)) else {
                    return;
                };
                audit.job = None;
                match result {
                    Ok(result) => {
                        if let AuditKind::Compare { sources, .. } = &mut audit.kind {
                            *sources = Some(result);
                        }
                    }
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn run_key_audit(&mut self, cx: &mut Context<Self>) {
        let Some(audit) = self.audit.as_mut() else {
            return;
        };
        if audit.job.is_some() || audit.keys.is_empty() {
            return;
        }
        enum Work {
            Duplicates(Arc<QueryResult>),
            Compare(Arc<VersionSources>, CompareProgress),
        }
        enum Answer {
            Duplicates(DuplicateRows),
            Compare(VersionDiff),
        }
        let work = match &mut audit.kind {
            AuditKind::Duplicates { source, result } => {
                if let Some(previous) = result.take() {
                    cx.background_executor()
                        .spawn(async move { drop(previous) })
                        .detach();
                }
                Work::Duplicates(source.clone())
            }
            AuditKind::Compare {
                sources: Some(sources),
                result,
                progress,
                ..
            } => {
                if let Some(previous) = result.take() {
                    cx.background_executor()
                        .spawn(async move { drop(previous) })
                        .detach();
                }
                *progress.lock().unwrap_or_else(|error| error.into_inner()) = None;
                Work::Compare(sources.clone(), progress.clone())
            }
            AuditKind::Compare { sources: None, .. } => return,
        };
        let keys = audit.keys.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let job = cancel.clone();
        let refresh = cancel.clone();
        audit.job = Some(cancel.clone());
        self.error = None;
        let task = cx.background_executor().spawn(async move {
            match work {
                Work::Duplicates(source) => {
                    duplicate_keys(&source, &keys, &cancel).map(Answer::Duplicates)
                }
                Work::Compare(sources, progress) => sources
                    .compare(&keys, &cancel, |done, total| {
                        *progress.lock().unwrap_or_else(|error| error.into_inner()) =
                            Some((done, total));
                    })
                    .map(Answer::Compare),
            }
        });
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(160))
                    .await;
                if weak
                    .update(cx, |this, cx| {
                        let pending = this
                            .audit
                            .as_ref()
                            .is_some_and(|audit| audit.owns_job(&refresh));
                        if pending {
                            cx.notify();
                        }
                        pending
                    })
                    .ok()
                    != Some(true)
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                let Some(audit) = this.audit.as_mut().filter(|audit| audit.owns_job(&job)) else {
                    return;
                };
                audit.job = None;
                audit.result_scroll = gpui::UniformListScrollHandle::new();
                match result {
                    Ok(Answer::Duplicates(rows)) => {
                        if let AuditKind::Duplicates { result, .. } = &mut audit.kind {
                            *result = Some(rows);
                        }
                    }
                    Ok(Answer::Compare(rows)) => {
                        if let AuditKind::Compare {
                            result,
                            show_schema,
                            ..
                        } = &mut audit.kind
                        {
                            *result = Some(rows);
                            *show_schema = false;
                        }
                    }
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn audit_title(&self) -> &'static str {
        match self.audit.as_ref().map(|audit| &audit.kind) {
            Some(AuditKind::Compare { .. }) => "Compare File Versions",
            _ => "Check Duplicate Keys",
        }
    }

    fn focus_duplicate_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(AuditControls {
            kind: AuditKind::Duplicates { source, .. },
            ..
        }) = &self.audit
        else {
            return;
        };
        if self
            .result
            .as_ref()
            .is_none_or(|result| !Arc::ptr_eq(result, source))
        {
            return;
        }
        let Some(view_row) = self.view_row(row) else {
            return;
        };
        let column = self.selection.map_or(0, |(_, (_, col))| col);
        self.close_data_audit(cx);
        self.selection_kind = SelectionKind::Cells;
        self.selecting = false;
        self.selection = Some(((view_row, column), (view_row, column)));
        self.reveal_row(view_row);
        self.reveal_column(column, window);
        self.focus.focus(window);
        cx.notify();
    }

    fn toggle_audit_key(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(audit) = self.audit.as_mut().filter(|audit| audit.job.is_none()) else {
            return;
        };
        if let Some(index) = audit.keys.iter().position(|name| *name == key) {
            audit.keys.remove(index);
        } else {
            audit.keys.push(key);
        }
        match &mut audit.kind {
            AuditKind::Duplicates { result, .. } => {
                if let Some(result) = result.take() {
                    cx.background_executor()
                        .spawn(async move { drop(result) })
                        .detach();
                }
            }
            AuditKind::Compare {
                result,
                show_schema,
                ..
            } => {
                if let Some(result) = result.take() {
                    cx.background_executor()
                        .spawn(async move { drop(result) })
                        .detach();
                }
                *show_schema = true;
            }
        }
        audit.result_scroll = gpui::UniformListScrollHandle::new();
        self.error = None;
        cx.notify();
    }

    fn show_audit_schema(&mut self, schema: bool, cx: &mut Context<Self>) {
        if let Some(audit) = &mut self.audit
            && let AuditKind::Compare { show_schema, .. } = &mut audit.kind
        {
            *show_schema = schema;
            audit.result_scroll = gpui::UniformListScrollHandle::new();
            cx.notify();
        }
    }

    pub(super) fn render_audit_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(audit) = &self.audit else {
            return div().into_any_element();
        };
        let c = Colors::new(self.preferences.dark);
        let dark = self.preferences.dark;
        let busy = audit.job.is_some();
        let choices = audit.key_choices(cx);
        let choice_count = choices.len();
        let compare_tabs = match &audit.kind {
            AuditKind::Compare {
                sources: Some(_),
                result,
                show_schema,
                ..
            } => Some((*show_schema, result.is_some())),
            _ => None,
        };
        let source_message = match &audit.kind {
            AuditKind::Duplicates { .. } => {
                "Checks the current result. Matching NULL key values are grouped together."
            }
            AuditKind::Compare { .. } => {
                "Compares both complete files. Choose a unique, non-NULL key. Matching fields with compatible types are compared."
            }
        };
        let result_count = match &audit.kind {
            AuditKind::Duplicates {
                result: Some(result),
                ..
            } => result.len(),
            AuditKind::Compare {
                sources: Some(sources),
                show_schema: true,
                ..
            } => sources.schema_changes.len(),
            AuditKind::Compare {
                result: Some(result),
                show_schema: false,
                ..
            } => result.len(),
            _ => 0,
        };
        let summary = match &audit.kind {
            AuditKind::Duplicates {
                result: Some(result),
                ..
            } => {
                format!(
                    "{} {}  ·  {} {}",
                    t("Duplicate groups"),
                    result.groups,
                    t("Rows with duplicate keys"),
                    result.len()
                )
            }
            AuditKind::Compare {
                sources: Some(sources),
                show_schema: true,
                ..
            } => {
                if sources.schema_changes.is_empty() {
                    t("No Schema differences.")
                } else {
                    format!(
                        "{} {}",
                        t("Schema differences"),
                        sources.schema_changes.len()
                    )
                }
            }
            AuditKind::Compare {
                result: Some(result),
                show_schema: false,
                ..
            } => [ChangeKind::Added, ChangeKind::Removed, ChangeKind::Modified]
                .iter()
                .zip(result.counts)
                .map(|(kind, count)| format!("{} {count}", t(kind.label())))
                .collect::<Vec<_>>()
                .join("  ·  "),
            _ => String::new(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.))
            .child(
                div()
                    .text_color(rgb(c.muted))
                    .whitespace_normal()
                    .child(t(source_message)),
            )
            .when_some(
                match &audit.kind {
                    AuditKind::Compare { other, .. } => Some(other),
                    _ => None,
                },
                |el, other| {
                    let original = self.info.as_ref().and_then(|info| info.paths.first());
                    el.child(
                        div()
                            .flex()
                            .gap_2()
                            .child(div().flex_1().min_w_0().truncate().child(format!(
                                    "{} {}",
                                    t("Original file"),
                                    original
                                        .and_then(|p| p.file_name())
                                        .map(|n| n.to_string_lossy())
                                        .unwrap_or_default()
                                )))
                            .child(div().flex_1().min_w_0().truncate().child(format!(
                                    "{} {}",
                                    t("Comparison file"),
                                    other
                                        .file_name()
                                        .map(|n| n.to_string_lossy())
                                        .unwrap_or_default()
                                ))),
                    )
                },
            )
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(t("Key fields")),
            )
            .child(
                div()
                    .h(px(28.))
                    .w_full()
                    .flex_shrink_0()
                    .child(audit.search.clone()),
            )
            .when(!audit.keys.is_empty(), |el| {
                el.child(div().text_color(rgb(c.muted)).truncate().child(format!(
                    "{} {}",
                    t("Selected keys"),
                    audit.keys.join(", ")
                )))
            })
            .when(choice_count == 0 && !busy, |el| {
                el.child(
                    div()
                        .text_color(rgb(c.muted))
                        .child(t("No matching scalar key fields.")),
                )
            })
            .when(choice_count > 0, |el| {
                el.child(
                    uniform_list(
                        "audit-key-fields",
                        choice_count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            let Some(audit) = &this.audit else {
                                return Vec::new();
                            };
                            let choices = audit.key_choices(cx);
                            range
                                .filter_map(|index| {
                                    let key = choices.get(index)?.to_string();
                                    let selected = audit.keys.contains(&key);
                                    Some(
                                        controls::menu_item(
                                            ("audit-key", index),
                                            c,
                                            selected,
                                            !busy,
                                        )
                                        .w_full()
                                        .h(px(26.))
                                        .when(!busy, |el| {
                                            el.on_click(cx.listener({
                                                let key = key.clone();
                                                move |this, _, _, cx| {
                                                    this.toggle_audit_key(key.clone(), cx)
                                                }
                                            }))
                                        })
                                        .child(icons::checkbox(selected, dark))
                                        .child(div().min_w_0().truncate().child(key))
                                        .into_any_element(),
                                    )
                                })
                                .collect()
                        }),
                    )
                    .h(px((choice_count.min(4) * 26) as f32))
                    .flex_shrink_0()
                    .border_1()
                    .border_color(rgb(c.border))
                    .track_scroll(audit.key_scroll.clone()),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(!busy && audit.keys.is_empty() && choice_count > 0, |el| {
                        el.child(
                            div()
                                .text_color(rgb(c.muted))
                                .child(t("Choose at least one key field.")),
                        )
                    })
                    .child(self.button(
                        "run-audit",
                        "Run",
                        !busy && !audit.keys.is_empty(),
                        |this, _, cx| this.run_key_audit(cx),
                        cx,
                    ))
                    .when_some(compare_tabs, |row, (show_schema, has_rows)| {
                        row.child(
                            controls::segment_group(c)
                                .child(
                                    controls::segment("audit-schema", c, show_schema)
                                        .child(t("Schema differences"))
                                        .when(!show_schema, |el| {
                                            el.on_click(cx.listener(|this, _, _, cx| {
                                                this.show_audit_schema(true, cx);
                                            }))
                                        }),
                                )
                                .when(has_rows, |tabs| {
                                    tabs.child(
                                        controls::segment("audit-rows", c, !show_schema)
                                            .child(t("Row changes"))
                                            .when(show_schema, |el| {
                                                el.on_click(cx.listener(|this, _, _, cx| {
                                                    this.show_audit_schema(false, cx);
                                                }))
                                            }),
                                    )
                                }),
                        )
                    })
                    .when(busy, |row| {
                        let progress = match &audit.kind {
                            AuditKind::Compare { progress, .. } => {
                                *progress.lock().unwrap_or_else(|error| error.into_inner())
                            }
                            _ => None,
                        };
                        let label = progress.map_or_else(
                            || t("Checking data…"),
                            |(done, total)| format!("{} {done}/{total}", t("Comparing fields")),
                        );
                        row.child(div().text_color(rgb(c.muted)).child(label))
                    }),
            )
            .when(!summary.is_empty(), |el| {
                el.child(div().text_color(rgb(c.accent)).child(summary))
            })
            .when(result_count > 0, |el| {
                el.child(self.render_audit_rows(audit, result_count, cx))
            })
            .into_any_element()
    }

    fn render_audit_rows(
        &self,
        audit: &AuditControls,
        count: usize,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let c = Colors::new(self.preferences.dark);
        let headings = match &audit.kind {
            AuditKind::Duplicates { .. } => ["Original row number", "Key occurrences", ""],
            AuditKind::Compare {
                show_schema: true, ..
            } => ["Field", "Original file", "Comparison file"],
            AuditKind::Compare { .. } => ["Change", "Original row number", "Comparison row number"],
        };
        let columns = |values: [String; 3]| {
            div().flex().gap_3().w_full().children(
                values
                    .into_iter()
                    .map(|value| div().flex_1().min_w_0().truncate().child(value)),
            )
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .border_1()
            .border_color(rgb(c.border))
            .child(
                div()
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .bg(rgb(c.bar))
                    .child(columns(headings.map(t))),
            )
            .child(
                uniform_list(
                    "audit-results",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        let Some(audit) = &this.audit else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|index| {
                                let (values, location) = match &audit.kind {
                                    AuditKind::Duplicates {
                                        source,
                                        result: Some(result),
                                    } => {
                                        let (row, count) = result.row(index)?;
                                        let original = source.source_row_number(row)?;
                                        (
                                            [
                                                original.to_string(),
                                                count.to_string(),
                                                t("Go to Row"),
                                            ],
                                            Some(row),
                                        )
                                    }
                                    AuditKind::Compare {
                                        sources: Some(sources),
                                        show_schema: true,
                                        ..
                                    } => {
                                        let change = sources.schema_changes.get(index)?;
                                        (
                                            [
                                                change.field.clone(),
                                                change.original.as_deref().unwrap_or("—").into(),
                                                change.revised.as_deref().unwrap_or("—").into(),
                                            ],
                                            None,
                                        )
                                    }
                                    AuditKind::Compare {
                                        result: Some(result),
                                        show_schema: false,
                                        ..
                                    } => {
                                        let change = result.row(index)?;
                                        let row_label = |row: Option<u64>| {
                                            row.map_or("—".into(), |row| row.to_string())
                                        };
                                        (
                                            [
                                                t(change.kind.label()),
                                                row_label(change.original_row),
                                                row_label(change.revised_row),
                                            ],
                                            None,
                                        )
                                    }
                                    _ => return None,
                                };
                                Some(
                                    div()
                                        .id(("audit-result", index))
                                        .w_full()
                                        .h(px(27.))
                                        .px_2()
                                        .flex()
                                        .items_center()
                                        .border_b_1()
                                        .border_color(rgb(c.grid))
                                        .when_some(location, |row, location| {
                                            row.cursor_pointer()
                                                .hover(|el| el.bg(rgb(c.hover)))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.focus_duplicate_row(
                                                            location, window, cx,
                                                        )
                                                    },
                                                ))
                                        })
                                        .child(columns(values))
                                        .into_any_element(),
                                )
                            })
                            .collect()
                    }),
                )
                .flex_1()
                .min_h_0()
                .track_scroll(audit.result_scroll.clone()),
            )
            .into_any_element()
    }
}
