//! Named, per-source view settings. Source data and Arrow buffers are never serialized.
use super::*;

use crate::engine::SortKey;
use anyhow::{Context as _, Result, ensure};
use arrow::datatypes::{DataType, Field, Schema};
use arrow_ipc::{convert::try_schema_from_ipc_buffer, writer::StreamWriter};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    collections::HashSet,
    fs,
    io::{self, Read},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

const STORE_VERSION: u32 = 1;
const MAX_STORE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SAVED_FROZEN_ROWS: usize = 16_384;
static STORE_LOCK: Mutex<()> = Mutex::new(());
type SourceKey = Vec<String>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedField {
    name: String,
    data_type: String,
    nullable: bool,
}

impl From<&ColumnInfo> for SavedField {
    fn from(column: &ColumnInfo) -> Self {
        Self {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            nullable: column.nullable,
        }
    }
}

fn type_label_matches(saved: &str, current: &str) -> bool {
    if saved == current {
        return true;
    }
    if !saved.contains("BIGINT") || !current.contains("INT64") {
        return false;
    }

    // Older views stored display labels alongside the exact Arrow schema.
    // Convert only unquoted type tokens, including nested field types.
    let separator = |ch: char| !ch.is_ascii_alphanumeric() && ch != '_';
    let mut legacy = String::with_capacity(current.len());
    for (index, part) in current.split('"').enumerate() {
        if index > 0 {
            legacy.push('"');
        }
        if index % 2 == 1 {
            legacy.push_str(part);
            continue;
        }
        for word in part.split_inclusive(separator) {
            let kind = word.trim_end_matches(separator);
            if kind == "INT64" {
                legacy.push_str("BIGINT");
                legacy.push_str(&word[kind.len()..]);
            } else {
                legacy.push_str(word);
            }
        }
    }
    saved == legacy
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileIdentity {
    device: u64,
    inode: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    length: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FrozenRows {
    files: Vec<FileIdentity>,
    original_rows: Vec<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedView {
    name: String,
    fields: Vec<SavedField>,
    arrow_schema: String,
    offset: usize,
    rows: usize,
    #[serde(default)]
    whole_source: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    load_to_end: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    csv_options: Option<CsvOptions>,
    filters: FilterGroup,
    draft: FilterGroup,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    draft_whole_source: Option<bool>,
    sort: Option<(String, bool)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sort_keys: Vec<SortKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    top_n: Option<usize>,
    widths: HashMap<String, f32>,
    frozen_columns: Vec<String>,
    frozen_rows: Option<FrozenRows>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn same_text_layout(saved: Option<&CsvOptions>, current: Option<&CsvOptions>) -> bool {
    match (saved, current) {
        (Some(saved), Some(current)) => {
            // Type inference settings affect the resulting schema, which is
            // separately checked against the saved exact Arrow schema.
            saved.delimiter == current.delimiter && saved.has_header == current.has_header
        }
        (None, None) => true,
        _ => false,
    }
}

fn saved_field_type(field: &Field) -> Field {
    field
        .clone()
        .with_data_type(saved_data_type(field.data_type()))
        .with_metadata(
            field
                .metadata()
                .iter()
                .filter(|(key, _)| {
                    matches!(
                        key.as_str(),
                        "ARROW:extension:name" | "ARROW:extension:metadata"
                    )
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
}

fn saved_data_type(data_type: &DataType) -> DataType {
    let field = |field: &Field| Arc::new(saved_field_type(field));
    match data_type {
        DataType::List(item) => DataType::List(field(item)),
        DataType::LargeList(item) => DataType::LargeList(field(item)),
        DataType::ListView(item) => DataType::ListView(field(item)),
        DataType::LargeListView(item) => DataType::LargeListView(field(item)),
        DataType::FixedSizeList(item, length) => DataType::FixedSizeList(field(item), *length),
        DataType::Map(entries, sorted) => DataType::Map(field(entries), *sorted),
        DataType::Struct(fields) => {
            DataType::Struct(fields.iter().map(|item| field(item)).collect())
        }
        DataType::Union(fields, mode) => DataType::Union(
            fields.iter().map(|(id, item)| (id, field(item))).collect(),
            *mode,
        ),
        DataType::Dictionary(key, value) => DataType::Dictionary(
            Box::new(saved_data_type(key)),
            Box::new(saved_data_type(value)),
        ),
        DataType::RunEndEncoded(ends, values) => {
            DataType::RunEndEncoded(field(ends), field(values))
        }
        other => other.clone(),
    }
}

fn selected_arrow_schema(schema: &Schema) -> Schema {
    Schema::new(
        schema
            .fields()
            .iter()
            .map(|field| saved_field_type(field))
            .collect::<Vec<_>>(),
    )
}

fn encode_arrow_schema(schema: &Schema) -> Result<String> {
    let mut bytes = Vec::new();
    StreamWriter::try_new(&mut bytes, &selected_arrow_schema(schema))?.finish()?;
    Ok(STANDARD.encode(bytes))
}

fn validate_arrow_schema(encoded: &str, current: &Schema) -> Result<()> {
    let bytes = STANDARD
        .decode(encoded)
        .context("Saved view contains an invalid Arrow Schema.")?;
    let saved = std::panic::catch_unwind(|| try_schema_from_ipc_buffer(&bytes))
        .map_err(|_| anyhow::anyhow!("Saved view contains an invalid Arrow Schema."))?
        .context("Saved view contains an invalid Arrow Schema.")?;
    ensure!(
        saved == selected_arrow_schema(current),
        "Saved view does not match the current Schema."
    );
    Ok(())
}

impl SavedView {
    fn draft_whole_source(&self) -> bool {
        self.draft_whole_source.unwrap_or(self.whole_source)
    }

    fn request(&self, info: &DatasetInfo) -> Result<QueryRequest> {
        ensure!(
            self.rows > 0 && self.rows <= i64::MAX as usize && self.offset <= i64::MAX as usize,
            "Saved view has an invalid loading range."
        );
        ensure!(
            !self.fields.is_empty(),
            "Saved view has no selected fields."
        );
        ensure!(
            self.frozen_rows
                .as_ref()
                .is_none_or(|rows| rows.original_rows.len() <= MAX_SAVED_FROZEN_ROWS),
            "Saved view contains too many frozen rows."
        );
        let available = info
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c))
            .collect::<HashMap<_, _>>();
        let mut seen = HashSet::new();
        let columns = self
            .fields
            .iter()
            .map(|saved| {
                let column = available
                    .get(saved.name.as_str())
                    .copied()
                    .filter(|column| {
                        type_label_matches(&saved.data_type, &column.data_type)
                            && column.nullable == saved.nullable
                    });
                ensure!(
                    seen.insert(saved.name.as_str()) && column.is_some(),
                    "Saved view does not match the current Schema."
                );
                Ok(column.expect("validated field").clone())
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            self.sort
                .as_ref()
                .is_none_or(|(name, _)| seen.contains(name.as_str())),
            "Saved view does not match the current Schema."
        );
        let mut sort_fields = HashSet::new();
        ensure!(
            self.sort_keys
                .iter()
                .all(|key| seen.contains(key.field.as_str())
                    && sort_fields.insert(key.field.as_str())),
            "Saved view does not match the current Schema."
        );
        ensure!(
            (self.sort.is_none() || self.sort_keys.is_empty())
                && self.top_n.is_none_or(
                    |rows| rows > 0 && (self.sort.is_some() || !self.sort_keys.is_empty())
                ),
            "Saved view contains invalid sorting settings."
        );
        ensure!(
            self.frozen_columns
                .iter()
                .all(|name| seen.contains(name.as_str()))
                && self
                    .widths
                    .iter()
                    .all(|(name, width)| seen.contains(name.as_str())
                        && (70. ..=1500.).contains(width)),
            "Saved view contains invalid column settings."
        );
        if !self.draft.conditions.is_empty() {
            // Draft values can be incomplete. Verify the field paths and
            // operators without requiring the draft to be ready to apply.
            let targets = crate::filtering::targets(&columns);
            ensure!(
                self.draft
                    .conditions
                    .iter()
                    .all(|condition| targets.iter().any(|target| {
                        target.field == condition.field
                            && target.path == condition.path
                            && crate::filtering::operators(&target.data_type)
                                .contains(&condition.operator)
                    })),
                "Saved view does not match the current Schema."
            );
        }
        let filter = self.filters.to_filter(&columns)?;
        Ok(QueryRequest {
            selected_columns: columns.into_iter().map(|column| column.name).collect(),
            filter,
            sort: self.sort.clone(),
            sort_keys: self.sort_keys.clone(),
            top_n: self.top_n,
            offset: self.offset,
            limit: self.rows,
            whole_source: self.whole_source,
            load_to_end: self.load_to_end,
            ..Default::default()
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    source: SourceKey,
    view: SavedView,
}

#[derive(Serialize, Deserialize)]
struct Store {
    version: u32,
    entries: Vec<Entry>,
}

impl Store {
    fn read(path: &Path) -> Result<Self> {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: STORE_VERSION,
                    entries: Vec::new(),
                });
            }
            Err(error) => return Err(error).context("Could not read saved views."),
        };
        let mut bytes = Vec::new();
        (&mut file)
            .take((MAX_STORE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_STORE_BYTES,
            "Saved views file is too large."
        );
        let store: Self = serde_json::from_slice(&bytes).context("Saved views file is invalid.")?;
        ensure!(
            store.version == STORE_VERSION,
            "Saved views file uses an unsupported version."
        );
        Ok(store)
    }

    fn write(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= MAX_STORE_BYTES,
            "Saved views file is too large."
        );
        fs::create_dir_all(
            path.parent()
                .context("Saved views need a parent directory.")?,
        )?;
        crate::source_guard::save_copy(path, &[], &bytes).context("Could not save views.")
    }

    fn list(&self, source: &SourceKey) -> Vec<SavedView> {
        let mut views: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| &entry.source == source)
            .map(|entry| entry.view.clone())
            .collect();
        views.sort_by(|left, right| left.name.cmp(&right.name));
        views
    }
}

fn store_path() -> PathBuf {
    Preferences::path().with_file_name("saved-views.json")
}

fn source_key(paths: &[PathBuf]) -> Result<SourceKey> {
    let mut key = paths
        .iter()
        .map(|path| {
            // Encode path bytes losslessly, including names that aren't UTF-8.
            fs::canonicalize(path).map(|path| STANDARD.encode(path.as_os_str().as_bytes()))
        })
        .collect::<io::Result<Vec<_>>>()
        .context("Could not identify the source for saved views.")?;
    key.sort_unstable();
    key.dedup();
    ensure!(!key.is_empty(), "Open a source before using saved views.");
    Ok(key)
}

fn file_identities(stamps: &SourceStamps, file_count: usize) -> Option<Vec<FileIdentity>> {
    // Older large directory snapshots track at most 250 timestamps. Only pin
    // positions when the entire ordered file list can be checked.
    if stamps.len() != file_count || file_count == 0 {
        return None;
    }
    stamps
        .iter()
        .map(|(path, expected)| {
            let metadata = fs::metadata(path).ok()?;
            let modified = metadata.modified().ok()?;
            let length = metadata.len();
            if Some((modified, length)) != *expected {
                return None;
            }
            Some(FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                length,
            })
        })
        .collect()
}

/// A validated request can publish its view state only with a successful query.
pub(super) struct Restore {
    view: SavedView,
    file_stamps: SourceStamps,
    file_count: usize,
    pinned_rows: Vec<usize>,
    notes: Vec<&'static str>,
}

impl Restore {
    pub(super) fn prepare_rows(&mut self, result: &QueryResult, cancel: &AtomicBool) -> Result<()> {
        ensure!(!cancel.load(Ordering::Relaxed), "Operation cancelled");
        validate_arrow_schema(&self.view.arrow_schema, &result.data.schema())?;
        let Some(frozen) = self.view.frozen_rows.take() else {
            return Ok(());
        };
        ensure!(
            frozen.original_rows.len() <= MAX_SAVED_FROZEN_ROWS,
            "Saved view contains too many frozen rows."
        );
        if file_identities(&self.file_stamps, self.file_count).as_ref() != Some(&frozen.files) {
            self.notes
                .push("Source files changed. Saved frozen rows were skipped.");
            return Ok(());
        }
        let mut wanted = frozen.original_rows.into_iter().collect::<HashSet<_>>();
        for row in 0..result.row_count() {
            if wanted.is_empty() {
                break;
            }
            if row % 1024 == 0 {
                ensure!(!cancel.load(Ordering::Relaxed), "Operation cancelled");
            }
            if result
                .source_row_number(row)
                .is_some_and(|number| wanted.remove(&number))
            {
                self.pinned_rows.push(row);
            }
        }
        if !wanted.is_empty() {
            self.notes
                .push("Some saved frozen rows are absent from this result.");
        }
        Ok(())
    }
}

pub(super) struct SavedViewsControls {
    session: Arc<()>,
    name: Entity<TextInput>,
    source: Option<SourceKey>,
    views: Vec<SavedView>,
    pending: bool,
    message: Option<String>,
    _name_subscription: Subscription,
}

struct StoreUpdate {
    source: SourceKey,
    views: Vec<SavedView>,
    message: Option<&'static str>,
}

impl Viewer {
    pub(super) fn open_saved_views(&mut self, cx: &mut Context<Self>) {
        if self.engine.is_none()
            || self.paths.is_empty()
            || self.busy.is_some()
            || self.panel.is_some()
        {
            return;
        }
        let dark = self.preferences.dark;
        let name = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(dark)
                .with_placeholder(&t("View name"))
        });
        let subscription = cx.subscribe(&name, |this, _, event, cx| {
            if matches!(event, InputEvent::Submit) {
                this.save_current_view(cx);
            }
        });
        self.saved_views = Some(SavedViewsControls {
            session: Arc::new(()),
            name,
            source: None,
            views: Vec::new(),
            pending: false,
            message: None,
            _name_subscription: subscription,
        });
        self.panel = Some(Panel::SavedViews);
        self.settings_menu = None;
        self.axis_menu = None;
        self.selection_menu = None;
        self.error = None;
        let paths = self.paths.clone();
        self.run_saved_views_task(
            move || {
                let source = source_key(&paths)?;
                let _lock = STORE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
                let views = Store::read(&store_path())?.list(&source);
                Ok(StoreUpdate {
                    source,
                    views,
                    message: None,
                })
            },
            cx,
        );
    }

    pub(super) fn saved_view_focus(&self, cx: &App) -> FocusHandle {
        if self.result.is_none() || self.aggregation_base.is_some() {
            return self.focus.clone();
        }
        self.saved_views
            .as_ref()
            .map_or_else(|| self.focus.clone(), |view| view.name.focus_handle(cx))
    }

    pub(super) fn saved_view_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.saved_views
            .as_ref()
            .is_some_and(|view| view.name.focus_handle(cx).is_focused(window))
    }

    pub(super) fn sync_saved_view_input(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.saved_views {
            view.name.update(cx, |input, cx| {
                input.set_dark(self.preferences.dark, cx);
                input.set_placeholder(&t("View name"), cx);
            });
        }
    }

    fn run_saved_views_task(
        &mut self,
        work: impl FnOnce() -> Result<StoreUpdate> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(controls) = &mut self.saved_views else {
            return;
        };
        controls.pending = true;
        controls.message = None;
        let session = controls.session.clone();
        let task = cx.background_executor().spawn(async move { work() });
        cx.spawn(async move |weak, cx| {
            let update = task.await;
            let _ = weak.update(cx, |this, cx| {
                let Some(controls) = &mut this.saved_views else {
                    return;
                };
                if !Arc::ptr_eq(&controls.session, &session) {
                    return;
                }
                controls.pending = false;
                match update {
                    Ok(update) => {
                        controls.source = Some(update.source);
                        controls.views = update.views;
                        controls.message = update.message.map(str::to_owned);
                    }
                    Err(error) => controls.message = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn saved_view_error(&mut self, error: impl std::fmt::Display, cx: &mut Context<Self>) {
        if let Some(controls) = &mut self.saved_views {
            controls.message = Some(error.to_string());
        }
        cx.notify();
    }

    fn save_current_view(&mut self, cx: &mut Context<Self>) {
        if self.panel != Some(Panel::SavedViews)
            || self.busy.is_some()
            || self.aggregation_base.is_some()
        {
            return;
        }
        let (Some(controls), Some(result), Some(info)) =
            (&self.saved_views, &self.result, &self.info)
        else {
            return;
        };
        let Some(source) = controls.source.clone().filter(|_| !controls.pending) else {
            return;
        };
        let name = controls.name.read(cx).text().trim().to_owned();
        if name.is_empty() || name.chars().count() > 80 {
            self.saved_view_error("Enter a view name from 1 to 80 characters.", cx);
            return;
        }
        let widths = result
            .columns
            .iter()
            .zip(self.widths.iter().copied())
            .filter(|(_, width)| (70. ..=1500.).contains(width))
            .map(|(column, width)| (column.name.clone(), width))
            .collect();
        let frozen_columns = result
            .columns
            .iter()
            .filter(|column| self.table_state.is_column_pinned(&column.name))
            .map(|column| column.name.clone())
            .collect();
        let draft = self.filter_editor.as_ref().map_or_else(
            || self.displayed_filters.clone(),
            |editor| editor.read(cx).snapshot_group(cx),
        );
        let mut view = SavedView {
            name,
            fields: result.columns.iter().map(SavedField::from).collect(),
            arrow_schema: String::new(),
            offset: self.displayed_request.offset,
            rows: self.displayed_request.limit,
            whole_source: self.displayed_request.whole_source,
            load_to_end: self.displayed_request.load_to_end,
            csv_options: self.csv_options.clone(),
            filters: self.displayed_filters.clone(),
            draft,
            draft_whole_source: self
                .filter_editor
                .as_ref()
                .map(|editor| editor.read(cx).whole_source()),
            sort: self.displayed_request.sort.clone(),
            sort_keys: self.displayed_request.sort_keys.clone(),
            top_n: self.displayed_request.top_n,
            widths,
            frozen_columns,
            frozen_rows: None,
        };
        if let Err(error) = view.request(info) {
            self.saved_view_error(error, cx);
            return;
        }
        let pins = self.view_rows.frozen();
        let pin_count = pins.len();
        let original_rows = (!self.modified && pin_count > 0 && pin_count <= MAX_SAVED_FROZEN_ROWS)
            .then(|| {
                pins.iter()
                    .map(|&row| result.source_row_number(row))
                    .collect::<Option<Vec<_>>>()
            })
            .flatten();
        let stamps = self.file_stamps.clone();
        let file_count = info.file_count;
        let schema = result.data.schema();
        self.run_saved_views_task(
            move || {
                view.arrow_schema = encode_arrow_schema(&schema)?;
                if let Some(original_rows) = original_rows
                    && let Some(files) = file_identities(&stamps, file_count)
                {
                    view.frozen_rows = Some(FrozenRows {
                        files,
                        original_rows,
                    });
                }
                let skipped = pin_count > 0 && view.frozen_rows.is_none();
                let _lock = STORE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
                let path = store_path();
                let mut store = Store::read(&path)?;
                ensure!(
                    !store
                        .entries
                        .iter()
                        .any(|entry| entry.source == source && entry.view.name == view.name),
                    "A saved view already has that name."
                );
                store.entries.push(Entry {
                    source: source.clone(),
                    view,
                });
                store.write(&path)?;
                let message = if skipped {
                    "View saved. Frozen rows could not be saved safely."
                } else {
                    "View saved."
                };
                Ok(StoreUpdate {
                    views: store.list(&source),
                    source,
                    message: Some(message),
                })
            },
            cx,
        );
    }

    fn delete_saved_view(&mut self, name: String, cx: &mut Context<Self>) {
        if self.panel != Some(Panel::SavedViews) {
            return;
        }
        let Some(controls) = &self.saved_views else {
            return;
        };
        let Some(source) = controls
            .source
            .clone()
            .filter(|_| !controls.pending && self.busy.is_none())
        else {
            return;
        };
        self.run_saved_views_task(
            move || {
                let _lock = STORE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
                let path = store_path();
                let mut store = Store::read(&path)?;
                store
                    .entries
                    .retain(|entry| entry.source != source || entry.view.name != name);
                store.write(&path)?;
                Ok(StoreUpdate {
                    views: store.list(&source),
                    source,
                    message: Some("Saved view removed."),
                })
            },
            cx,
        );
    }

    fn restore_saved_view(&mut self, index: usize, window: &Window, cx: &mut Context<Self>) {
        if self.panel != Some(Panel::SavedViews) || self.busy.is_some() {
            return;
        }
        if self.modified {
            self.saved_view_error("Reload the source before applying a saved view.", cx);
            return;
        }
        let (Some(controls), Some(info)) = (&self.saved_views, &self.info) else {
            return;
        };
        let Some(mut view) = controls
            .views
            .get(index)
            .cloned()
            .filter(|_| !controls.pending)
        else {
            return;
        };
        if !same_text_layout(view.csv_options.as_ref(), self.csv_options.as_ref()) {
            self.saved_view_error(
                "Reopen the text file with its saved separator and header settings.",
                cx,
            );
            return;
        }
        let request = match view.request(info) {
            Ok(request) => request,
            Err(error) => {
                self.saved_view_error(error, cx);
                return;
            }
        };
        let mut notes = Vec::new();
        let names: Vec<_> = request
            .selected_columns
            .iter()
            .map(String::as_str)
            .collect();
        let defaults = crate::table_state::reconcile_column_widths(&[], &[], &names);
        let frozen = view
            .frozen_columns
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let required = names
            .iter()
            .zip(defaults)
            .filter(|(name, _)| frozen.contains(**name))
            .map(|(name, default)| view.widths.get(*name).copied().unwrap_or(default))
            .sum::<f32>();
        let maximum = f32::from(window.viewport_size().width)
            - self.right_sidebar_width(window)
            - 24.
            - 180.
            - 52.;
        if !frozen.is_empty() && required > maximum {
            view.frozen_columns.clear();
            notes.push("Saved frozen columns do not fit this window.");
        }
        let filters = view.filters.clone();
        let restore = Restore {
            view,
            file_stamps: self.file_stamps.clone(),
            file_count: info.file_count,
            pinned_rows: Vec::new(),
            notes,
        };
        self.execute_request(request, filters, Some(restore), cx);
    }

    pub(super) fn commit_saved_view(&mut self, restore: Restore, cx: &mut Context<Self>) {
        let Restore {
            view,
            pinned_rows,
            notes,
            ..
        } = restore;
        let Some(result) = &self.result else { return };
        self.chosen_fields = result
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect();
        self.fields_explicitly_chosen = true;
        let names: Vec<_> = self.chosen_fields.iter().map(String::as_str).collect();
        let defaults = crate::table_state::reconcile_column_widths(&[], &[], &names);
        self.widths = names
            .iter()
            .zip(defaults)
            .map(|(name, default)| view.widths.get(*name).copied().unwrap_or(default))
            .collect();
        self.offset
            .update(cx, |input, cx| input.set_text(&view.offset.to_string(), cx));
        self.count.update(cx, |input, cx| {
            input.set_text(&self.displayed_request.limit.to_string(), cx)
        });
        let draft_whole_source = view.draft_whole_source();
        self.visual_filters = view.filters;
        if self.filter_editor.is_some()
            || self.filter_sidebar_open
            || view.draft != self.visual_filters
            || draft_whole_source != self.request.whole_source
        {
            self.install_filter_editor(view.draft, cx);
            if let Some(editor) = &self.filter_editor {
                editor.update(cx, |editor, cx| {
                    editor.set_whole_source(draft_whole_source, cx)
                });
            }
        }
        self.table_state.clear_pins();
        self.table_state.pin_columns(view.frozen_columns);
        self.table_state.pin_rows(pinned_rows);
        self.selecting_initial_fields = false;
        self.close_fields_sidebar();
        self.panel = None;
        self.saved_views = None;
        self.notice = std::iter::once(t("Saved view applied."))
            .chain(notes.into_iter().map(t))
            .collect::<Vec<_>>()
            .join(" ");
    }

    pub(super) fn render_saved_views_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(controls) = &self.saved_views else {
            return div().into_any_element();
        };
        let c = Colors::new(self.preferences.dark);
        let ready = !controls.pending && self.busy.is_none() && controls.source.is_some();
        let can_save = self.result.is_some() && self.aggregation_base.is_none();
        div().flex().flex_col().min_h_0().flex_1().gap_3()
            .child(div().text_color(rgb(c.muted)).child(t("Saved views store the fields, range, filters, sorting, widths and safely addressable frozen positions for this source.")))
            .when(can_save, |el| el.child(div().flex().gap_2().items_center()
                .child(self.input_shell(&controls.name, None))
                .child(self.button("save-view", "Save Current View", ready, |this, _, cx| this.save_current_view(cx), cx))))
            .when(self.aggregation_base.is_some(), |el| el.child(div().text_color(rgb(c.warning)).child(t("Return to the full result before saving a view."))))
            .when(controls.pending, |el| el.child(t("Reading or saving views…")))
            .when(self.busy.is_some(), |el| el.child(div().flex().gap_2().items_center()
                .child(t("Applying saved view…"))
                .child(self.button("cancel-view-restore", "Cancel", true, |this, _, cx| this.cancel(cx), cx))))
            .when(controls.views.is_empty() && ready, |el| el.child(div().text_color(rgb(c.muted)).child(t("No saved views for this source."))))
            .when(!controls.views.is_empty(), |el| el.child(
                div().id("saved-views-list").flex_1().min_h_0().overflow_y_scroll()
                    .border_1().border_color(rgb(c.border)).rounded(px(3.))
                    .children(controls.views.iter().enumerate().map(|(index, view)| {
                        let name = view.name.clone();
                        div().id(("saved-view-row", index)).flex().items_center().gap_2().px_2().py_1().min_h(px(34.))
                            .border_b_1().border_color(rgb(c.border))
                            .child(div().flex_1().min_w_0().truncate().child(view.name.clone()))
                            .child(self.button("restore-view", "Restore", ready, move |this, window, cx| this.restore_saved_view(index, window, cx), cx))
                            .child(self.button("remove-view", "Remove", ready, move |this, _, cx| this.delete_saved_view(name.clone(), cx), cx))
                    }))
            ))
            .when_some(controls.message.as_ref(), |el, message| el.child(div().text_color(rgb(c.muted)).child(t(message))))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::Cell,
        filtering::{FilterCondition, FilterOperator},
    };

    fn id_column() -> ColumnInfo {
        ColumnInfo {
            name: "id".into(),
            data_type: "BIGINT".into(),
            nullable: false,
        }
    }

    fn view() -> SavedView {
        let filters = FilterGroup {
            conditions: vec![FilterCondition {
                field: "id".into(),
                operator: FilterOperator::Ge,
                value: "1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut draft = filters.clone();
        draft.conditions[0].value.clear();
        SavedView {
            name: "Regression review".into(),
            fields: vec![SavedField::from(&id_column())],
            arrow_schema: encode_arrow_schema(
                &crate::columnar::fixture_schema(&[id_column()]).unwrap(),
            )
            .unwrap(),
            offset: 99,
            rows: 4,
            whole_source: false,
            load_to_end: false,
            csv_options: None,
            filters,
            draft,
            draft_whole_source: None,
            sort: Some(("id".into(), true)),
            sort_keys: Vec::new(),
            top_n: None,
            widths: HashMap::from([("id".into(), 176.)]),
            frozen_columns: vec!["id".into()],
            frozen_rows: None,
        }
    }

    #[test]
    fn saved_views_restore_new_sort_settings_and_read_legacy_single_keys() -> Result<()> {
        let mut view = view();
        let mut legacy = serde_json::to_value(&view)?;
        legacy.as_object_mut().unwrap().remove("whole_source");
        let legacy: SavedView = serde_json::from_value(legacy)?;
        let other = ColumnInfo {
            name: "other".into(),
            ..id_column()
        };
        let info = DatasetInfo {
            columns: vec![id_column(), other.clone()],
            ..Default::default()
        };
        assert!(!legacy.draft_whole_source());
        let legacy = legacy.request(&info)?;
        assert_eq!(
            legacy.sort_rules().collect::<Vec<_>>(),
            [("id", true, false)]
        );
        assert!(legacy.top_n.is_none());
        assert!(!legacy.whole_source);

        view.fields.push(SavedField::from(&other));
        view.whole_source = true;
        let fallback: SavedView = serde_json::from_slice(&serde_json::to_vec(&view)?)?;
        assert!(fallback.draft_whole_source());
        view.draft_whole_source = Some(false);
        view.sort = None;
        view.sort_keys = vec![
            SortKey {
                field: "other".into(),
                descending: true,
                nulls_first: true,
            },
            SortKey {
                field: "id".into(),
                descending: false,
                nulls_first: false,
            },
        ];
        view.top_n = Some(3);
        let mut restored: SavedView = serde_json::from_slice(&serde_json::to_vec(&view)?)?;
        let request = restored.request(&info)?;
        assert_eq!(
            request.sort_rules().collect::<Vec<_>>(),
            [("other", true, true), ("id", false, false)]
        );
        assert_eq!(request.top_n, Some(3));
        assert!(request.whole_source);
        assert!(!restored.draft_whole_source());
        restored.sort_keys[0].field = "id".into();
        assert!(restored.request(&info).is_err());
        Ok(())
    }

    #[test]
    fn saved_views_round_trip_per_source_and_keep_an_incomplete_draft_separate() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source_a = temp.path().join("first.parquet");
        let source_b = temp.path().join("second.parquet");
        fs::write(&source_a, b"first fixture")?;
        fs::write(&source_b, b"second fixture")?;
        let first = source_key(&[source_a])?;
        let second = source_key(&[source_b])?;
        let path = temp.path().join("views.json");
        assert!(Store::read(&path)?.entries.is_empty());
        let view = view();
        Store {
            version: STORE_VERSION,
            entries: vec![Entry {
                source: first.clone(),
                view: view.clone(),
            }],
        }
        .write(&path)?;
        let store = Store::read(&path)?;
        assert!(store.list(&second).is_empty());
        let views = store.list(&first);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].draft, view.draft);
        let info = DatasetInfo {
            columns: vec![id_column()],
            ..Default::default()
        };
        let request = views[0].request(&info)?;
        assert_eq!(request.filter, view.filters.to_filter(&info.columns)?);
        assert_eq!(request.sort, view.sort);
        assert_eq!((request.offset, request.limit), (99, 4));
        assert_eq!(views[0].widths["id"], 176.);
        let changed = DatasetInfo {
            columns: vec![ColumnInfo {
                data_type: "VARCHAR".into(),
                ..id_column()
            }],
            ..Default::default()
        };
        assert!(views[0].request(&changed).is_err());
        Ok(())
    }

    #[test]
    fn saved_views_accept_legacy_bigint_labels_for_the_same_arrow_types() -> Result<()> {
        let mut view = view();
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new(
                "details",
                DataType::Struct(
                    [
                        Arc::new(Field::new("BIGINT \"INT64\"", DataType::Int64, true)),
                        Arc::new(Field::new("count", DataType::UInt64, true)),
                    ]
                    .into(),
                ),
                true,
            ),
        ]);
        let info = DatasetInfo {
            columns: crate::arrow_types::columns_from_schema(&schema),
            ..Default::default()
        };
        view.fields.push(SavedField {
            name: "details".into(),
            data_type: r#"STRUCT("BIGINT ""INT64""" BIGINT, "count" UBIGINT)"#.into(),
            nullable: true,
        });
        view.arrow_schema = encode_arrow_schema(&schema)?;
        assert_eq!(view.request(&info)?.selected_columns, ["id", "details"]);
        validate_arrow_schema(&view.arrow_schema, &schema)?;
        view.fields[1].data_type = r#"STRUCT("BIGINT ""INT64""" BIGINT, "count" BIGINT)"#.into();
        assert!(view.request(&info).is_err());
        Ok(())
    }

    #[test]
    fn saved_text_view_layout_checks_only_separator_and_header() {
        let saved = CsvOptions {
            delimiter: b';',
            has_header: false,
            ..Default::default()
        };
        let mut current = saved.clone();
        current.sample_records = 5_000;
        assert!(same_text_layout(Some(&saved), Some(&current)));
        current.has_header = true;
        assert!(!same_text_layout(Some(&saved), Some(&current)));
        current.has_header = false;
        current.delimiter = b'|';
        assert!(!same_text_layout(Some(&saved), Some(&current)));
    }

    #[test]
    fn frozen_rows_restore_by_original_position_only_for_the_same_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("frozen.parquet");
        fs::write(&path, b"row identity fixture")?;
        let metadata = fs::metadata(&path)?;
        let stamps = vec![(path.clone(), Some((metadata.modified()?, metadata.len())))];
        let files = file_identities(&stamps, 1).expect("unchanged fixture");
        let mut view = view();
        view.frozen_rows = Some(FrozenRows {
            files,
            original_rows: vec![100, 103],
        });
        let mut result = QueryResult::from_cells(
            vec![id_column()],
            (0..4)
                .map(|row| {
                    vec![Cell {
                        text: row.to_string(),
                        ..Default::default()
                    }]
                })
                .collect(),
        );
        result.data = result
            .data
            .with_source_row_start(100)
            .with_row_indices(arrow::array::UInt64Array::from(vec![2, 3, 0, 1]))?;
        let restore = |view| Restore {
            view,
            file_stamps: stamps.clone(),
            file_count: 1,
            pinned_rows: Vec::new(),
            notes: Vec::new(),
        };
        let mut unchanged = restore(view.clone());
        unchanged.prepare_rows(&result, &AtomicBool::new(false))?;
        assert_eq!(unchanged.pinned_rows, vec![1, 2]);
        assert!(unchanged.notes.is_empty());
        fs::write(&path, b"changed")?;
        let mut changed = restore(view);
        changed.prepare_rows(&result, &AtomicBool::new(false))?;
        assert!(changed.pinned_rows.is_empty());
        assert!(!changed.notes.is_empty());
        Ok(())
    }

    #[test]
    fn saved_arrow_types_distinguish_same_label_but_ignore_unrelated_metadata() -> Result<()> {
        let field = Field::new("measurement", DataType::Float16, false);
        let recorded = Schema::new(vec![
            field
                .clone()
                .with_metadata(HashMap::from([("note".into(), "old".into())])),
        ]);
        let encoded = encode_arrow_schema(&recorded)?;
        let renamed_note = Schema::new(vec![
            field
                .clone()
                .with_metadata(HashMap::from([("note".into(), "new".into())])),
        ]);
        validate_arrow_schema(&encoded, &renamed_note)?;
        let different_type = Schema::new(vec![field.clone().with_data_type(DataType::Float32)]);
        assert_eq!(
            crate::arrow_types::columns_from_schema(&recorded),
            crate::arrow_types::columns_from_schema(&different_type)
        );
        assert!(validate_arrow_schema(&encoded, &different_type).is_err());
        let extension = Schema::new(vec![field.with_metadata(HashMap::from([(
            "ARROW:extension:name".into(),
            "custom.type".into(),
        )]))]);
        assert!(validate_arrow_schema(&encoded, &extension).is_err());
        Ok(())
    }
}
