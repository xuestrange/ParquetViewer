//! Owned Arrow windows with DataFusion index-only filtering and sorting.
//! Source data is read once per selected window. Immutable views and snapshots
//! share its buffers; DataFusion returns only base row indices.
use crate::{
    columnar::{ArrowTable, RowIndexChunk, RowIndexWidth},
    source_loader::{ParquetSource, SourceStamps, filter_loaded_indices},
};
use anyhow::{Context, Result, bail, ensure};
#[cfg(test)]
use arrow::array::{Array, ArrayRef};
use arrow::{
    array::Int64Array,
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use datafusion::{
    catalog::streaming::StreamingTable,
    execution::TaskContext,
    physical_plan::{
        SendableRecordBatchStream, stream::RecordBatchStreamAdapter, streaming::PartitionStream,
    },
    prelude::{SessionConfig, SessionContext, col},
};
use futures::TryStreamExt;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, task::AbortHandle};

#[path = "csv_source.rs"]
pub(crate) mod csv_source;
use csv_source::{CsvOptions, CsvPrepared, CsvSource};

pub(crate) fn has_delimited_text_extension(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        ["csv", "tsv", "tab", "txt", "psv"]
            .iter()
            .any(|candidate| ext.eq_ignore_ascii_case(candidate))
    })
}

pub(crate) fn is_parquet_file(path: &Path) -> bool {
    use std::io::Read;
    let extension = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if [".parquet", ".parquet.gz", ".parquet.gzip"]
        .iter()
        .any(|suffix| extension.ends_with(suffix))
    {
        return true;
    }
    let mut magic = [0_u8; 4];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == *b"PAR1"
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct DatasetInfo {
    pub paths: Vec<PathBuf>,
    pub columns: Vec<ColumnInfo>,
    pub row_count: usize,
    /// False means `row_count` is only a prefix-based estimate, never a full scan.
    pub row_count_known: bool,
    pub file_count: usize,
    pub file_size: u64,
    pub row_groups: usize,
    /// Original field names disabled because another field differs only by case.
    pub excluded_fields: Vec<String>,
    /// Nonfatal messages suitable for the UI; excluded fields never enter `data`.
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SortKey {
    pub field: String,
    pub descending: bool,
    pub nulls_first: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryRequest {
    /// Empty means all columns. UI field selection should disallow selecting none.
    pub selected_columns: Vec<String>,
    pub filter: String,
    /// Evaluate the visual predicate against the complete source before import.
    pub whole_source: bool,
    /// Retained for saved request compatibility; nonempty advanced SQL is disabled.
    pub sql: Option<String>,
    /// Legacy single-field sort: column name and descending flag.
    pub sort: Option<(String, bool)>,
    /// Keys in decreasing priority, applied to the loaded, filtered window.
    pub sort_keys: Vec<SortKey>,
    /// Cap the sorted result without shrinking the imported source range.
    pub top_n: Option<usize>,
    pub offset: usize,
    pub limit: usize,
    /// Read to EOF when the CSV's total record count is still unknown.
    pub load_to_end: bool,
}

impl Default for QueryRequest {
    fn default() -> Self {
        Self {
            selected_columns: Vec::new(),
            filter: String::new(),
            whole_source: false,
            sql: None,
            sort: None,
            sort_keys: Vec::new(),
            top_n: None,
            offset: 0,
            limit: 1000,
            load_to_end: false,
        }
    }
}

impl QueryRequest {
    pub fn sort_rules(&self) -> impl Iterator<Item = (&str, bool, bool)> {
        self.sort_keys
            .iter()
            .map(|key| (key.field.as_str(), key.descending, key.nulls_first))
            .chain(
                self.sort
                    .iter()
                    .filter(|_| self.sort_keys.is_empty())
                    .map(|(field, descending)| (field.as_str(), *descending, false)),
            )
    }

    pub fn set_sort(&mut self, keys: Vec<SortKey>, top_n: Option<usize>) {
        self.sort = None;
        self.sort_keys = keys;
        self.top_n = top_n;
    }

    pub fn retain_sort_fields(&mut self, fields: &[String]) {
        if self
            .sort
            .as_ref()
            .is_some_and(|(field, _)| !fields.contains(field))
        {
            self.sort = None;
        }
        self.sort_keys.retain(|key| fields.contains(&key.field));
        if self.sort.is_none() && self.sort_keys.is_empty() {
            self.top_n = None;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueryPhase {
    #[default]
    Loading,
    Indexing,
    Preparing,
    Filtering,
    Sorting,
    Aggregating,
    FinishingAggregation,
    Charting,
    Complete,
}

#[derive(Clone, Debug, Default)]
pub struct QueryProgressSnapshot {
    pub phase: QueryPhase,
    /// Actual progress within the current phase; absent when the execution engine cannot
    /// estimate its pipeline. Never inferred from elapsed time or file size.
    pub fraction: Option<f64>,
    pub processed_rows: usize,
    pub total_rows: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct QueryProgress(Arc<Mutex<QueryProgressSnapshot>>);

impl QueryProgress {
    pub fn snapshot(&self) -> QueryProgressSnapshot {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn update(&self, snapshot: QueryProgressSnapshot) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = snapshot;
    }

    pub(crate) fn begin_phase(&self, phase: QueryPhase) {
        self.update(QueryProgressSnapshot {
            phase,
            ..Default::default()
        });
    }

    fn advance_rows(&self, phase: QueryPhase, rows: usize, total: usize) {
        let mut current = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if current.phase == phase {
            current.processed_rows = current.processed_rows.max(rows);
            current.total_rows = Some(total);
            current.fraction = Some(if total == 0 {
                1.
            } else {
                (current.processed_rows as f64 / total as f64).clamp(0., 1.)
            });
        }
    }

    fn index_bytes(&self, bytes: u64, total: u64, to_end: bool) {
        let mut current = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if matches!(current.phase, QueryPhase::Loading | QueryPhase::Filtering)
            && current.processed_rows > 0
            && !to_end
        {
            return;
        }
        *current = QueryProgressSnapshot {
            phase: QueryPhase::Indexing,
            fraction: (total > 0).then(|| (bytes as f64 / total as f64).min(1.)),
            processed_rows: current.processed_rows,
            total_rows: current.total_rows,
        };
    }

    fn csv_decoded_rows(&self, rows: usize, total: Option<usize>, to_end: bool, filtering: bool) {
        let mut current = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let rows = current.processed_rows.max(rows);
        if to_end && current.phase == QueryPhase::Indexing {
            current.processed_rows = rows;
            current.total_rows = total;
            return;
        }
        *current = QueryProgressSnapshot {
            phase: if filtering {
                QueryPhase::Filtering
            } else {
                QueryPhase::Loading
            },
            fraction: total.map(|total| {
                if total == 0 {
                    1.
                } else {
                    (rows as f64 / total as f64).min(1.)
                }
            }),
            processed_rows: rows,
            total_rows: total,
        };
    }
}

#[derive(Clone, Debug, Default)]
pub struct Cell {
    pub text: String,
    pub is_null: bool,
    pub bytes: Option<Vec<u8>>,
    pub json: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // Detailed stage counters are consumed by CLI profiling.
pub struct QueryTimings {
    pub snapshot_ms: f64,
    pub metadata_ms: f64,
    pub arrow_start_ms: f64,
    pub arrow_fetch_ms: f64,
    pub adapter_ms: f64,
    pub window_load_ms: f64,
    pub index_query_ms: f64,
    pub window_cache_hit: bool,
}

#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub data: crate::columnar::ArrowTable,
    /// Total records in the source dataset, before paging or filtering.
    pub total_rows: usize,
    pub total_rows_known: bool,
    /// Number of imported records, before in-memory filtering. A whole-source
    /// filter imports only its matches.
    pub loaded_rows: usize,
    /// Matches when filtering reduced the count. A loaded-range Top N can
    /// mask a reduction; the count is then omitted without another scan.
    pub filtered_rows: Option<usize>,
    pub elapsed_ms: u128,
    pub snapshot_id: String,
    #[allow(dead_code)] // Read by reproducible CLI profiling, not by rendering.
    pub timings: QueryTimings,
}

impl QueryResult {
    /// One-based position in the original, lexically ordered dataset, retained
    /// through filtering/sorting. Advanced derived SQL has no source identity.
    pub fn source_row_number(&self, row: usize) -> Option<usize> {
        self.data.source_row_number(row)
    }
    pub fn row_count(&self) -> usize {
        self.data.row_count()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub fn is_null(&self, row: usize, col: usize) -> bool {
        self.data.is_null(row, col)
    }
    pub fn is_nested(&self, row: usize, col: usize) -> bool {
        self.data.is_nested(row, col)
    }
    pub fn binary(&self, row: usize, col: usize) -> Option<&[u8]> {
        self.data.binary(row, col)
    }
    pub fn display_text(&self, row: usize, col: usize) -> String {
        self.data.display_text(row, col)
    }
    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.data.cell(row, col)
    }
    pub fn preview_cell(&self, row: usize, col: usize, image_only: bool) -> Cell {
        self.data.preview_cell(row, col, image_only)
    }
    pub fn slice_cell(&self, row: usize, col: usize) -> Option<crate::columnar::ArrowTable> {
        self.data.slice_cell(row, col)
    }
    #[cfg(test)]
    pub fn from_cells(columns: Vec<ColumnInfo>, rows: Vec<Vec<Cell>>) -> Self {
        let data = crate::columnar::ArrowTable::from_cells(columns.clone(), rows);
        Self {
            columns,
            data,
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    Parquet,
    Xlsx,
    Xls,
}

#[derive(Default)]
pub struct InterruptHandle {
    active: Mutex<Option<ActiveOperation>>,
}
struct ActiveOperation {
    cancel: Weak<AtomicBool>,
    abort: Option<AbortHandle>,
}

impl InterruptHandle {
    pub fn interrupt(&self) {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(active) = active.as_ref() {
            if let Some(cancel) = active.cancel.upgrade() {
                cancel.store(true, Ordering::Release);
            }
            if let Some(abort) = &active.abort {
                abort.abort();
            }
        }
    }

    fn set_abort(&self, cancel: &Arc<AtomicBool>, abort: AbortHandle) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(active) = active.as_mut()
            && Weak::ptr_eq(&active.cancel, &Arc::downgrade(cancel))
        {
            if cancel.load(Ordering::Acquire) {
                abort.abort();
            }
            active.abort = Some(abort);
        }
    }

    fn run<T>(
        &self,
        external: &AtomicBool,
        action: impl FnOnce(&Arc<AtomicBool>) -> Result<T>,
    ) -> Result<T> {
        check_cancel(external)?;
        let cancel = Arc::new(AtomicBool::new(false));
        *self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(ActiveOperation {
            cancel: Arc::downgrade(&cancel),
            abort: None,
        });
        let _registration = OperationRegistration {
            handle: self,
            cancel: cancel.clone(),
        };
        // The public synchronous API accepts a borrowed AtomicBool. Bridge it
        // into the owned operation token used by source workers and Tokio tasks.
        std::thread::scope(|scope| {
            let (finished, receiver) = std::sync::mpsc::channel::<()>();
            let monitor = scope.spawn(move || {
                loop {
                    if external.load(Ordering::Acquire) {
                        self.interrupt();
                    }
                    match receiver.recv_timeout(Duration::from_millis(10)) {
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            });
            let result = action(&cancel);
            let _ = finished.send(());
            monitor
                .join()
                .map_err(|_| anyhow::anyhow!("Cancellation monitor failed"))?;
            result
        })
    }
}
struct OperationRegistration<'a> {
    handle: &'a InterruptHandle,
    cancel: Arc<AtomicBool>,
}
impl Drop for OperationRegistration<'_> {
    fn drop(&mut self) {
        let mut active = self
            .handle
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.cancel, &Arc::downgrade(&self.cancel)))
        {
            *active = None;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowKey {
    columns: Vec<String>,
    offset: usize,
    limit: usize,
    source_predicate: Option<String>,
}
impl WindowKey {
    fn same_range(&self, other: &Self) -> bool {
        self.offset == other.offset
            && self.limit == other.limit
            && self.source_predicate == other.source_predicate
    }
}
#[derive(Clone)]
struct WindowCache {
    key: WindowKey,
    // Only the selected fields belong to the active window.
    base: ArrowTable,
    context: Option<(SessionContext, bool)>,
    index_name: String,
    sort_names: Arc<HashMap<String, String>>,
    interval_sort_targets: Vec<String>,
}

impl WindowCache {
    fn new(key: WindowKey, base: ArrowTable, columns: &[ColumnInfo]) -> Self {
        Self {
            key,
            base,
            context: None,
            index_name: unique_index_name(columns),
            sort_names: Arc::new(HashMap::new()),
            interval_sort_targets: Vec::new(),
        }
    }
}
struct QueryContext {
    session: SessionContext,
    index_name: String,
    index_width: RowIndexWidth,
    sort_names: Arc<HashMap<String, String>>,
}

/// Keep only shared source arrays and small batch offsets in the provider.
/// The synthetic identity and compatibility sort keys are produced one batch
/// at a time when DataFusion polls a partition, never cached for the full table.
#[derive(Debug)]
struct IndexedPartition {
    schema: arrow::datatypes::SchemaRef,
    base: ArrowTable,
    batches: std::ops::Range<usize>,
    offsets: Arc<Vec<u64>>,
    adapted_columns: Arc<Vec<(usize, bool)>>,
    index_width: RowIndexWidth,
}

impl PartitionStream for IndexedPartition {
    fn schema(&self) -> &arrow::datatypes::SchemaRef {
        &self.schema
    }

    fn execute(&self, _context: Arc<TaskContext>) -> SendableRecordBatchStream {
        let batches = self.batches.clone();
        let base = self.base.clone();
        let offsets = self.offsets.clone();
        let adapted_columns = self.adapted_columns.clone();
        let schema = self.schema.clone();
        let index_width = self.index_width;
        let stream = futures::stream::iter(batches.map(move |index| {
            let produce = || -> Result<RecordBatch> {
                let batch = &base.raw_batches()[index];
                let offset = offsets[index];
                let end = offset
                    .checked_add(u64::try_from(batch.num_rows())?)
                    .context("Row index overflow")?;
                let mut arrays = batch.columns().to_vec();
                arrays.push(index_width.positions(offset..end).into_array());
                for &(column, normalize_interval) in adapted_columns.iter() {
                    let interval = if normalize_interval {
                        crate::parquet_compat::interval_sort_array(
                            batch.schema().field(column),
                            batch.column(column),
                        )?
                    } else {
                        batch.column(column).clone()
                    };
                    arrays.push(crate::columnar::sortable_array(&interval)?);
                }
                Ok(RecordBatch::try_new(schema.clone(), arrays)?)
            };
            produce().map_err(|error| datafusion::error::DataFusionError::External(error.into()))
        }));
        Box::pin(RecordBatchStreamAdapter::new(self.schema.clone(), stream))
    }
}
#[derive(Clone)]
struct Snapshot {
    window_key: WindowKey,
    columns: Vec<ColumnInfo>,
    data: ArrowTable,
    // The validated scalar predicate for general aggregation input replay.
    filter: String,
    sort_keys: Vec<SortKey>,
    /// Replaying the predicate alone would include rows outside this view.
    top_n: Option<usize>,
}

const SPARSE_AGGREGATE_MAX_ROWS: usize = 8192;
const SPARSE_AGGREGATE_MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// A small selection can reuse its known row membership for aggregation.
/// Limit gathered values to one batch of flat, exact types and cap text input
/// using retained buffer sizes. The ordinary replay path covers other inputs.
fn sparse_aggregate_projection(
    snapshot: &Snapshot,
    request: &crate::aggregation::AggregateRequest,
) -> Option<Vec<usize>> {
    let rows = snapshot.data.row_count();
    let base_rows = snapshot.data.base_row_count();
    if snapshot.filter.is_empty()
        || base_rows < 100_000
        || rows == 0
        || rows > SPARSE_AGGREGATE_MAX_ROWS
        || rows > base_rows / 256
    {
        return None;
    }
    let schema = snapshot.data.schema();
    let mut projection = Vec::new();
    let mut text_bytes = 0usize;
    for field in request.group_by.iter().chain(
        request
            .metrics
            .iter()
            .filter_map(|metric| metric.field.as_ref()),
    ) {
        let index = schema.index_of(field).ok()?;
        if projection.contains(&index) {
            continue;
        }
        if projection.len() == 8 {
            return None;
        }
        match schema.field(index).data_type() {
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => {
                text_bytes =
                    text_bytes.checked_add(snapshot.data.base_column_memory_size(index)?)?;
                if text_bytes > SPARSE_AGGREGATE_MAX_TEXT_BYTES {
                    return None;
                }
            }
            DataType::Float16 | DataType::Float32 | DataType::Float64 => return None,
            DataType::Boolean | DataType::Null => {}
            kind if kind.is_primitive() => {}
            _ => return None,
        }
        projection.push(index);
    }
    Some(projection)
}

#[derive(Debug)]
enum AggregateRange {
    DisplayedRows(std::ops::Range<usize>),
    SourceBatches(std::ops::Range<usize>),
}

#[derive(Debug)]
struct AggregateInput {
    data: ArrowTable,
    schema: arrow::datatypes::SchemaRef,
    range: AggregateRange,
    cancel: Arc<AtomicBool>,
    progress: QueryProgress,
    scanned: Arc<std::sync::atomic::AtomicUsize>,
    total: usize,
}

impl PartitionStream for AggregateInput {
    fn schema(&self) -> &arrow::datatypes::SchemaRef {
        &self.schema
    }

    fn execute(&self, _: Arc<TaskContext>) -> SendableRecordBatchStream {
        let data = self.data.clone();
        let cancel = self.cancel.clone();
        let scanned = self.scanned.clone();
        let progress = self.progress.clone();
        let total = self.total;
        let (range, step, end, indexed) = match &self.range {
            AggregateRange::DisplayedRows(rows) => {
                (rows.clone(), SPARSE_AGGREGATE_MAX_ROWS, rows.end, true)
            }
            AggregateRange::SourceBatches(batches) => (batches.clone(), 1, batches.end, false),
        };
        let stream = futures::stream::iter(range.step_by(step).map(move |start| {
            let next = || -> Result<RecordBatch> {
                check_cancel(&cancel)?;
                let batch = if indexed {
                    data.materialize_range(start..start.saturating_add(step).min(end))?
                } else {
                    data.raw_batches()[start].clone()
                };
                let rows =
                    scanned.fetch_add(batch.num_rows(), Ordering::Relaxed) + batch.num_rows();
                progress.advance_rows(QueryPhase::Aggregating, rows, total);
                Ok(batch)
            };
            next().map_err(|error| datafusion::error::DataFusionError::External(error.into()))
        }));
        Box::pin(RecordBatchStreamAdapter::new(self.schema.clone(), stream))
    }
}

enum DatasetSource {
    Parquet(ParquetSource),
    Csv(Arc<CsvSource>),
}

impl DatasetSource {
    fn pool(&self) -> &rayon::ThreadPool {
        match self {
            Self::Parquet(source) => source.pool(),
            Self::Csv(source) => source.pool(),
        }
    }

    #[cfg(test)]
    fn set_test_batch_rows(&self, rows: usize) {
        match self {
            Self::Parquet(source) => source.set_test_batch_rows(rows),
            Self::Csv(_) => panic!("This batch-size override is only for Parquet"),
        }
    }
}

pub struct Engine {
    source: DatasetSource,
    resources: crate::resource_budget::Client,
    pub info: DatasetInfo,
    snapshots: HashMap<String, Snapshot>,
    current_snapshot: Option<String>,
    next_snapshot: u64,
    window: Option<WindowCache>,
    // Drop cached contexts before their runtime. Every engine has its own
    // target partitions, memory workspace and CPU workers; no global budget.
    runtime: Option<Runtime>,
    interrupt: Arc<InterruptHandle>,
    // Preserve the single-worker Engine contract. The independently shared
    // interrupt handle remains thread-safe; Arc<Mutex<Engine>> is still Send.
    _single_worker: std::marker::PhantomData<std::cell::Cell<()>>,
}

fn worker_count(available: usize) -> usize {
    available.max(1)
}
pub(crate) fn workspace_bytes(workers: usize) -> usize {
    workers.saturating_mul(256).clamp(2048, 4096) * 1024 * 1024
}

impl Engine {
    #[allow(dead_code)]
    pub fn open(inputs: &[PathBuf]) -> Result<Self> {
        Self::open_cancellable(inputs, &AtomicBool::new(false))
    }
    pub fn open_cancellable(inputs: &[PathBuf], cancel: &AtomicBool) -> Result<Self> {
        check_cancel(cancel)?;
        if let Some(path) = inputs
            .iter()
            .find(|path| has_delimited_text_extension(path) && !path.is_dir())
        {
            ensure!(inputs.len() == 1, "Text files must be opened individually.");
            return Self::open_csv(path, None, cancel);
        }
        let parquet = ParquetSource::open(inputs, cancel)?;
        let info = parquet.info.clone();
        Self::with_source(DatasetSource::Parquet(parquet), info)
    }

    pub(crate) fn open_csv(
        path: &Path,
        options: Option<CsvOptions>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let csv = CsvSource::open(path, options, cancel, |_, _| {})?;
        Self::with_csv(csv)
    }

    pub(crate) fn open_prepared_csv(
        path: &Path,
        prepared: CsvPrepared,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::with_csv(CsvSource::open_from_prepared(path, prepared, cancel)?)
    }

    fn with_csv(csv: CsvSource) -> Result<Self> {
        let info = csv.info.clone();
        Self::with_source(DatasetSource::Csv(Arc::new(csv)), info)
    }

    pub(crate) fn start_csv_index(&self) -> Result<csv_source::CsvBrowse> {
        match &self.source {
            DatasetSource::Csv(source) => source.start_browse(),
            DatasetSource::Parquet(_) => anyhow::bail!("Expected a delimited text source"),
        }
    }

    /// Seed the ordinary snapshot cache with the bounded first text page. Small
    /// complete files can immediately use all existing analysis commands.
    pub(crate) fn open_csv_view(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<(csv_source::CsvBrowse, QueryRequest, QueryResult)> {
        let started = Instant::now();
        let DatasetSource::Csv(source) = &self.source else {
            anyhow::bail!("Expected a delimited text source");
        };
        let browse = source.start_browse()?;
        let rows = source.info.row_count.clamp(1, 1000);
        let data = browse.read_page(0, rows, 1024 * 1024, &self.resources, cancel)?;
        let progress = browse.progress();
        self.info.row_count = progress.indexed_rows;
        self.info.row_count_known = progress.complete;
        let request = QueryRequest {
            selected_columns: self
                .info
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            limit: data.row_count().max(1),
            ..Default::default()
        };
        let key = WindowKey {
            columns: request.selected_columns.clone(),
            offset: 0,
            limit: request.limit,
            source_predicate: None,
        };
        self.window = Some(WindowCache::new(key, data, &self.info.columns));
        let mut result = self.query_cancellable(&request, cancel)?;
        result.elapsed_ms = started.elapsed().as_millis();
        Ok((browse, request, result))
    }

    fn with_source(source: DatasetSource, info: DatasetInfo) -> Result<Self> {
        Ok(Self {
            source,
            info,
            resources: crate::resource_budget::Client::new(),
            snapshots: HashMap::new(),
            current_snapshot: None,
            next_snapshot: 0,
            window: None,
            runtime: None,
            interrupt: Arc::new(InterruptHandle::default()),
            _single_worker: std::marker::PhantomData,
        })
    }
    pub fn resource_client(&self) -> crate::resource_budget::Client {
        self.resources.clone()
    }
    #[allow(dead_code)]
    pub(crate) fn file_stamps(&self, limit: usize) -> SourceStamps {
        match &self.source {
            DatasetSource::Parquet(source) => source.file_stamps(limit),
            DatasetSource::Csv(source) => source.file_stamps(limit),
        }
    }
    pub fn interrupt_handle(&self) -> Arc<InterruptHandle> {
        self.interrupt.clone()
    }
    #[allow(dead_code)]
    pub fn query(&mut self, request: &QueryRequest) -> Result<QueryResult> {
        self.query_cancellable(request, &AtomicBool::new(false))
    }
    pub fn query_cancellable(
        &mut self,
        request: &QueryRequest,
        cancel: &AtomicBool,
    ) -> Result<QueryResult> {
        self.query_cancellable_with_progress(request, cancel, &QueryProgress::default())
    }
    pub fn query_cancellable_with_progress(
        &mut self,
        request: &QueryRequest,
        cancel: &AtomicBool,
        progress: &QueryProgress,
    ) -> Result<QueryResult> {
        let interrupt = self.interrupt.clone();
        let previous_window = self.window.clone();
        let result = interrupt.run(cancel, |operation| {
            self.query_inner(request, operation, cancel, progress)
        });
        if result.is_err() {
            self.window = previous_window;
        } else {
            drop(previous_window);
        }
        self.release_idle_query_resources();
        result
    }

    /// A completed background query can be rejected before it reaches the
    /// table. Recover the displayed snapshot's base without rereading Parquet.
    pub fn restore_displayed_window(&mut self, snapshot_id: &str) {
        let Some(snapshot) = self.snapshots.get(snapshot_id) else {
            return;
        };
        if self
            .window
            .as_ref()
            .is_none_or(|window| window.key != snapshot.window_key)
        {
            self.window = Some(WindowCache::new(
                snapshot.window_key.clone(),
                snapshot.data.without_row_indices(),
                &snapshot.columns,
            ));
        }
        self.current_snapshot = Some(snapshot_id.to_owned());
        self.release_idle_query_resources();
    }

    fn query_inner(
        &mut self,
        request: &QueryRequest,
        cancel: &Arc<AtomicBool>,
        external_cancel: &AtomicBool,
        progress: &QueryProgress,
    ) -> Result<QueryResult> {
        let started = Instant::now();
        check_cancel(cancel)?;
        ensure!(request.limit > 0, "Record count must be greater than zero.");
        ensure!(
            request.offset <= i64::MAX as usize && request.limit <= i64::MAX as usize,
            "Record count and offset must not exceed 9,223,372,036,854,775,807."
        );
        let csv = matches!(&self.source, DatasetSource::Csv(_));
        ensure!(
            !request.load_to_end || csv,
            "An open-ended row range requires a delimited text file."
        );
        ensure!(
            request.sql.as_ref().is_none_or(|sql| sql.trim().is_empty()),
            "Advanced SQL is no longer supported. Use the visual filters."
        );
        let predicate = validated_filter(&request.filter)?;
        let source_predicate = request.whole_source.then_some(predicate.as_str());
        ensure!(
            source_predicate.is_none_or(|predicate| !predicate.is_empty()),
            "Add a filter condition before scanning the entire file."
        );
        let filter = if request.whole_source {
            String::new()
        } else {
            predicate.clone()
        };
        let selected = if request.selected_columns.is_empty() {
            self.info
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect::<Vec<_>>()
        } else {
            request.selected_columns.clone()
        };
        ensure!(!selected.is_empty(), "Choose at least one field.");
        let mut seen = HashSet::new();
        let columns = selected
            .iter()
            .map(|name| {
                ensure!(seen.insert(name), "Duplicate selected field: {name}");
                self.info
                    .columns
                    .iter()
                    .find(|column| &column.name == name)
                    .cloned()
                    .with_context(|| format!("Selected field is not available: {name}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut sort_fields = HashSet::new();
        for (name, _, _) in request.sort_rules() {
            ensure!(sort_fields.insert(name), "Duplicate sort field: {name}");
            ensure!(
                selected.iter().any(|field| field == name),
                "Sort column is not loaded: {name}"
            );
        }
        ensure!(
            request.sort.is_none() || request.sort_keys.is_empty(),
            "Conflicting sort rules"
        );
        ensure!(
            request
                .top_n
                .is_none_or(|rows| rows > 0 && !sort_fields.is_empty()),
            "Top N needs a positive row count and at least one sort field."
        );
        let key = WindowKey {
            columns: selected.clone(),
            offset: if request.whole_source {
                0
            } else {
                request.offset
            },
            limit: if csv && (request.load_to_end || request.whole_source) {
                usize::MAX
            } else if request.whole_source {
                self.info.row_count
            } else {
                request.limit
            },
            source_predicate: source_predicate.map(str::to_owned),
        };
        let previous = self
            .window
            .as_ref()
            .filter(|window| window.key.same_range(&key));
        let same_selection = previous.is_some_and(|window| window.key.columns == selected);
        let missing = if same_selection {
            Vec::new()
        } else if let Some(window) = previous {
            let schema = window.base.schema();
            let available = schema
                .fields()
                .iter()
                .map(|field| field.name().as_str())
                .collect::<HashSet<_>>();
            selected
                .iter()
                .filter(|name| !available.contains(name.as_str()))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            selected.clone()
        };
        let cache_hit = previous.is_some() && missing.is_empty();
        let mut timings = QueryTimings {
            window_cache_hit: cache_hit,
            ..Default::default()
        };
        let mut projected_result = None;
        if !same_selection {
            let cached = previous.map(|window| window.base.clone());
            let available_base = if missing.is_empty() {
                cached.context("Arrow field cache is unavailable")?
            } else {
                let expected = self
                    .info
                    .row_count
                    .saturating_sub(key.offset)
                    .min(key.limit);
                let loaded_at = Instant::now();
                // Decode only fields absent from the same-range cache. Existing
                // Arrow arrays remain owned and are merged by shared slices even
                // when the two reads produce different batch boundaries.
                let added = match &self.source {
                    DatasetSource::Parquet(source) => {
                        let phase = if key.source_predicate.is_some() {
                            QueryPhase::Filtering
                        } else {
                            QueryPhase::Loading
                        };
                        progress.update(QueryProgressSnapshot {
                            phase,
                            fraction: Some(0.),
                            processed_rows: 0,
                            total_rows: Some(expected),
                        });
                        let on_rows = |rows| progress.advance_rows(phase, rows, expected);
                        if let Some(predicate) = &key.source_predicate {
                            let expr = physical_visual_filter(predicate, source.schema())?;
                            source.load_filtered(
                                &missing,
                                expr,
                                &self.resources,
                                cancel,
                                on_rows,
                            )?
                        } else {
                            source.load_window(
                                &missing,
                                key.offset,
                                key.limit,
                                &self.resources,
                                cancel,
                                on_rows,
                            )?
                        }
                    }
                    DatasetSource::Csv(source) => {
                        let to_end = key.limit == usize::MAX && !source.row_count().1;
                        let scanning_filter = key.source_predicate.is_some();
                        let on_bytes = |bytes, total| progress.index_bytes(bytes, total, to_end);
                        let on_rows = |rows| {
                            let (total, known) = source.row_count();
                            let total = if known {
                                Some(total.saturating_sub(key.offset).min(key.limit))
                            } else {
                                (key.limit != usize::MAX).then_some(key.limit)
                            };
                            progress.csv_decoded_rows(rows, total, to_end, scanning_filter);
                        };
                        if let Some(predicate) = &key.source_predicate {
                            let expr = physical_visual_filter(predicate, source.schema())?;
                            source.load_filtered(
                                &missing,
                                expr,
                                &self.resources,
                                cancel,
                                on_rows,
                                on_bytes,
                            )?
                        } else {
                            source.load_window(
                                &missing,
                                key.offset,
                                key.limit,
                                &self.resources,
                                cancel,
                                on_rows,
                                on_bytes,
                            )?
                        }
                    }
                };
                check_cancel(cancel)?;
                timings.window_load_ms = loaded_at.elapsed().as_secs_f64() * 1000.;
                timings.arrow_fetch_ms = timings.window_load_ms;
                if let Some(cached) = cached {
                    cached.merge_columns(&added)?
                } else {
                    added
                }
            };
            let schema = available_base.schema();
            let available = schema
                .fields()
                .iter()
                .enumerate()
                .map(|(index, field)| (field.name().as_str(), index))
                .collect::<HashMap<_, _>>();
            let indices = selected
                .iter()
                .map(|name| {
                    available
                        .get(name.as_str())
                        .copied()
                        .context("Cached field is unavailable")
                })
                .collect::<Result<Vec<_>>>()?;
            if cache_hit
                && let Some(snapshot) = self
                    .current_snapshot
                    .as_ref()
                    .and_then(|id| self.snapshots.get(id))
                    .filter(|snapshot| {
                        previous.is_some_and(|window| window.key == snapshot.window_key)
                            && snapshot.filter == filter
                            && snapshot.top_n
                                == request
                                    .top_n
                                    .filter(|rows| *rows < available_base.row_count())
                            && snapshot
                                .sort_keys
                                .iter()
                                .map(|key| (key.field.as_str(), key.descending, key.nulls_first))
                                .eq(request.sort_rules())
                    })
            {
                projected_result = Some(snapshot.data.project_columns(&indices)?);
            }
            let base = match &projected_result {
                Some(data) => data.without_row_indices(),
                None => available_base.project_columns(&indices)?,
            };
            check_cancel(cancel)?;
            self.window = Some(WindowCache::new(key, base, &columns));
        }
        let base = self
            .window
            .as_ref()
            .context("Arrow window is unavailable")?
            .base
            .clone();
        let loaded_rows = base.row_count();
        let top_n = request.top_n.filter(|rows| *rows < loaded_rows);
        if sort_fields.is_empty()
            && let Some(window) = self.window.as_mut()
        {
            // Only sorting needs the provider and compatibility sort keys.
            window.context = None;
            window.interval_sort_targets.clear();
            window.sort_names = Arc::new(HashMap::new());
        }
        let data = if let Some(data) = projected_result {
            if !filter.is_empty() {
                // Preserve validation if an API caller projects away a field
                // used by the predicate. The UI already prevents this.
                physical_visual_filter(&filter, base.schema())?;
            }
            data
        } else if filter.is_empty() && sort_fields.is_empty() {
            progress.update(QueryProgressSnapshot {
                phase: QueryPhase::Preparing,
                fraction: Some(1.),
                processed_rows: loaded_rows,
                total_rows: Some(loaded_rows),
            });
            base
        } else if sort_fields.is_empty() {
            progress.update(QueryProgressSnapshot {
                phase: QueryPhase::Filtering,
                fraction: Some(0.),
                processed_rows: 0,
                total_rows: Some(loaded_rows),
            });
            let context_at = Instant::now();
            let expr = physical_visual_filter(&filter, base.schema())?;
            timings.arrow_start_ms = context_at.elapsed().as_secs_f64() * 1000.;
            let query_at = Instant::now();
            let indices =
                filter_loaded_indices(self.source.pool(), &base, expr.as_ref(), cancel, |rows| {
                    progress.advance_rows(QueryPhase::Filtering, rows, loaded_rows);
                })?;
            timings.index_query_ms = query_at.elapsed().as_secs_f64() * 1000.;
            let adapted_at = Instant::now();
            let view = match indices {
                Some(indices) => base.with_row_index_batches(indices)?,
                None => base,
            };
            timings.adapter_ms = adapted_at.elapsed().as_secs_f64() * 1000.;
            view
        } else {
            let phase = QueryPhase::Sorting;
            progress.update(QueryProgressSnapshot {
                phase,
                fraction: None,
                processed_rows: 0,
                total_rows: None,
            });
            let context_at = Instant::now();
            let QueryContext {
                session: context,
                index_name,
                index_width,
                sort_names,
            } = self.filter_context(cancel, &sort_fields, predicate.is_empty())?;
            timings.arrow_start_ms = context_at.elapsed().as_secs_f64() * 1000.;
            let mut sql = format!(
                "SELECT {} FROM loaded_window",
                quote_identifier(&index_name)
            );
            if !filter.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&filter);
            }
            // Original source positions make equal sort keys stable.
            sql.push_str(" ORDER BY ");
            for (name, descending, nulls_first) in request.sort_rules() {
                let quoted = quote_identifier(sort_names.get(name).map_or(name, String::as_str));
                let text = columns
                    .iter()
                    .find(|column| column.name == name)
                    .is_some_and(|column| {
                        matches!(
                            column.data_type.to_ascii_uppercase().as_str(),
                            "VARCHAR" | "TEXT" | "STRING"
                        )
                    });
                // Preserve the viewer's case-insensitive Text ordering through
                // DataFusion/Arrow expressions, without a Rust row comparator.
                let key = if text {
                    format!("lower({quoted})")
                } else {
                    quoted
                };
                sql.push_str(&format!(
                    "{} {} NULLS {}, ",
                    key,
                    if descending { "DESC" } else { "ASC" },
                    if nulls_first { "FIRST" } else { "LAST" },
                ));
            }
            sql.push_str(&format!("{} ASC", quote_identifier(&index_name)));
            if let Some(rows) = top_n {
                sql.push_str(&format!(" LIMIT {rows}"));
            }
            let query_at = Instant::now();
            let indices = self.query_indices(
                context,
                sql,
                index_width,
                cancel.clone(),
                progress.clone(),
                phase,
            )?;
            timings.index_query_ms = query_at.elapsed().as_secs_f64() * 1000.;
            let adapted_at = Instant::now();
            let view = base.with_row_index_batches(indices)?;
            timings.adapter_ms = adapted_at.elapsed().as_secs_f64() * 1000.;
            view
        };
        check_cancel(cancel)?;
        check_cancel(external_cancel)?;
        self.next_snapshot = self
            .next_snapshot
            .checked_add(1)
            .context("Snapshot sequence overflow")?;
        let snapshot_id = format!("arrow_snapshot_{}", self.next_snapshot);
        self.snapshots.insert(
            snapshot_id.clone(),
            Snapshot {
                window_key: self.window.as_ref().unwrap().key.clone(),
                columns: columns.clone(),
                data: data.clone(),
                filter: filter.clone(),
                sort_keys: request
                    .sort_rules()
                    .map(|(field, descending, nulls_first)| SortKey {
                        field: field.to_owned(),
                        descending,
                        nulls_first,
                    })
                    .collect(),
                top_n,
            },
        );
        self.current_snapshot = Some(snapshot_id.clone());
        if let DatasetSource::Csv(source) = &self.source {
            (self.info.row_count, self.info.row_count_known) = source.row_count();
        }
        let rows = data.row_count();
        let filtered_rows = if request.whole_source {
            (self.info.row_count_known && loaded_rows < self.info.row_count).then_some(loaded_rows)
        } else {
            (!filter.is_empty() && rows < top_n.unwrap_or(loaded_rows)).then_some(rows)
        };
        progress.update(QueryProgressSnapshot {
            phase: QueryPhase::Complete,
            fraction: Some(1.),
            processed_rows: rows,
            total_rows: Some(rows),
        });
        Ok(QueryResult {
            columns,
            data,
            total_rows: self.info.row_count,
            total_rows_known: self.info.row_count_known,
            loaded_rows,
            filtered_rows,
            elapsed_ms: started.elapsed().as_millis(),
            snapshot_id,
            timings,
        })
    }

    fn ensure_query_runtime(&mut self, cancel: &AtomicBool) -> Result<usize> {
        check_cancel(cancel)?;
        let workers =
            worker_count(std::thread::available_parallelism().map_or(1, |count| count.get()));
        if self.runtime.is_none() {
            self.runtime = Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(workers)
                    .thread_name("parquet-datafusion")
                    .enable_all()
                    .build()
                    .context("Could not start DataFusion CPU workers")?,
            );
        }
        Ok(workers)
    }

    fn filter_context(
        &mut self,
        cancel: &AtomicBool,
        sort: &HashSet<&str>,
        allow_spill: bool,
    ) -> Result<QueryContext> {
        let workers = self.ensure_query_runtime(cancel)?;
        let _entered = self.runtime.as_ref().unwrap().enter();
        let window = self
            .window
            .as_mut()
            .context("Arrow window is unavailable")?;
        let source_schema = window.base.schema();
        let index_width = RowIndexWidth::for_rows(window.base.row_count());
        let interval_sort_targets = source_schema
            .fields()
            .iter()
            .filter(|field| {
                sort.contains(field.name().as_str())
                    && crate::parquet_compat::contains_interval(field)
            })
            .map(|field| field.name().clone())
            .collect::<Vec<_>>();
        if window.interval_sort_targets != interval_sort_targets
            || window
                .context
                .as_ref()
                .is_some_and(|(_, cached_allow_spill)| *cached_allow_spill != allow_spill)
        {
            // A new sort shape needs a fresh provider, not another source read.
            // Releasing a prior interval sort also releases its numeric keys.
            window.context = None;
        }
        if window.context.is_none() {
            let memory_bytes = workspace_bytes(workers);
            let environment = if allow_spill {
                crate::datafusion_session::lazy_spill_runtime(memory_bytes)?
            } else {
                crate::datafusion_session::memory_only_runtime(memory_bytes)?
            };
            let config = SessionConfig::new()
                .with_target_partitions(workers)
                .with_batch_size(32768);
            let context = crate::datafusion_session::new_context(config, environment);
            let mut fields = source_schema.fields().to_vec();
            fields.push(Arc::new(Field::new(
                &window.index_name,
                index_width.data_type(),
                false,
            )));
            let mut used_names = fields
                .iter()
                .map(|field| field.name().to_ascii_lowercase())
                .collect::<HashSet<_>>();
            let mut sort_names = HashMap::new();
            let mut adapted_columns = Vec::new();
            for (column, field) in source_schema.fields().iter().enumerate() {
                let normalize_interval = interval_sort_targets.contains(field.name());
                let interval = if normalize_interval {
                    crate::parquet_compat::interval_sort_type(field)
                } else {
                    field.data_type().clone()
                };
                let sortable = crate::columnar::sortable_type(&interval);
                if &sortable != field.data_type() {
                    let mut alias = format!("__pv_sort_key_{column}");
                    while used_names.contains(&alias.to_ascii_lowercase()) {
                        alias.push('_');
                    }
                    used_names.insert(alias.to_ascii_lowercase());
                    fields.push(Arc::new(Field::new(&alias, sortable, field.is_nullable())));
                    sort_names.insert(field.name().clone(), alias);
                    adapted_columns.push((column, normalize_interval));
                }
            }
            let schema = Arc::new(Schema::new_with_metadata(
                fields,
                source_schema.metadata().clone(),
            ));
            let adapted_columns = Arc::new(adapted_columns);
            let mut offset = 0u64;
            let batch_count = window.base.raw_batches().len();
            let mut offsets = Vec::with_capacity(batch_count);
            for batch in window.base.raw_batches() {
                check_cancel(cancel)?;
                offsets.push(offset);
                offset = offset
                    .checked_add(u64::try_from(batch.num_rows())?)
                    .context("Row index overflow")?;
            }
            let offsets = Arc::new(offsets);
            let partition_count = workers.min(batch_count.max(1));
            let per_partition = batch_count.max(1).div_ceil(partition_count);
            let partitions = (0..batch_count.max(1))
                .step_by(per_partition)
                .map(|start| {
                    Arc::new(IndexedPartition {
                        schema: schema.clone(),
                        base: window.base.clone(),
                        batches: start..start.saturating_add(per_partition).min(batch_count),
                        offsets: offsets.clone(),
                        adapted_columns: adapted_columns.clone(),
                        index_width,
                    }) as Arc<dyn PartitionStream>
                })
                .collect();
            let table = StreamingTable::try_new(schema, partitions)?
                .with_sort_order(vec![col(&window.index_name).sort(true, false)]);
            context.register_table("loaded_window", Arc::new(table))?;
            window.sort_names = Arc::new(sort_names);
            window.interval_sort_targets = interval_sort_targets;
            window.context = Some((context, allow_spill));
        }
        Ok(QueryContext {
            session: window.context.as_ref().unwrap().0.clone(),
            index_name: window.index_name.clone(),
            index_width,
            sort_names: window.sort_names.clone(),
        })
    }

    fn query_indices(
        &self,
        context: SessionContext,
        sql: String,
        index_width: RowIndexWidth,
        cancel: Arc<AtomicBool>,
        progress: QueryProgress,
        phase: QueryPhase,
    ) -> Result<Vec<RowIndexChunk>> {
        let runtime = self
            .runtime
            .as_ref()
            .context("DataFusion runtime is unavailable")?;
        let operation = cancel.clone();
        let task = runtime.spawn(async move {
            check_cancel(&operation)?;
            let frame = context.sql(&sql).await?;
            let mut stream = frame.execute_stream().await?;
            let mut arrays = Vec::new();
            let mut rows = 0usize;
            while let Some(batch) = stream.try_next().await? {
                check_cancel(&operation)?;
                ensure!(
                    batch.num_columns() == 1,
                    "DataFusion returned an invalid row-index schema"
                );
                rows = rows
                    .checked_add(batch.num_rows())
                    .context("Filtered row count overflow")?;
                // Cloning a typed Arrow array shares this stream batch's
                // buffers. Keep the chunks instead of allocating a second,
                // full-size concatenated result-index array after streaming.
                arrays.push(index_width.retain_chunk(batch.column(0).as_ref())?);
                progress.update(QueryProgressSnapshot {
                    phase,
                    fraction: None,
                    processed_rows: rows,
                    total_rows: None,
                });
            }
            check_cancel(&operation)?;
            Ok::<_, anyhow::Error>(arrays)
        });
        self.interrupt.set_abort(&cancel, task.abort_handle());
        match runtime.block_on(task) {
            Ok(result) => result,
            Err(error) if error.is_cancelled() || cancel.load(Ordering::Acquire) => {
                bail!("Operation cancelled")
            }
            Err(error) => Err(anyhow::anyhow!("DataFusion execution failed: {error}")),
        }
    }

    #[allow(dead_code)] // Also used by command-line benchmarks and regression tests.
    pub fn aggregate_snapshot_cancellable(
        &mut self,
        snapshot_id: &str,
        request: &crate::aggregation::AggregateRequest,
        cancel: &AtomicBool,
    ) -> Result<crate::aggregation::AggregateResult> {
        self.aggregate_snapshot_with_progress(
            snapshot_id,
            request,
            cancel,
            &QueryProgress::default(),
        )
    }

    /// Stream the immutable displayed snapshot to DataFusion in shared batches.
    pub fn aggregate_snapshot_with_progress(
        &mut self,
        snapshot_id: &str,
        request: &crate::aggregation::AggregateRequest,
        cancel: &AtomicBool,
        progress: &QueryProgress,
    ) -> Result<crate::aggregation::AggregateResult> {
        progress.begin_phase(QueryPhase::Aggregating);
        let snapshot = self
            .snapshots
            .get(snapshot_id)
            .context("The requested result snapshot is no longer available. Reload the file.")?
            .clone();
        let interrupt = self.interrupt.clone();
        let result = interrupt.run(cancel, |operation| {
            self.aggregate_snapshot_inner(snapshot, request, operation.clone(), progress)
        });
        self.release_idle_query_resources();
        let result = result?;
        progress.update(QueryProgressSnapshot {
            phase: QueryPhase::Complete,
            fraction: Some(1.),
            processed_rows: result.input_rows,
            total_rows: Some(result.input_rows),
        });
        Ok(result)
    }

    fn aggregate_snapshot_inner(
        &mut self,
        snapshot: Snapshot,
        request: &crate::aggregation::AggregateRequest,
        cancel: Arc<AtomicBool>,
        progress: &QueryProgress,
    ) -> Result<crate::aggregation::AggregateResult> {
        let started = Instant::now();
        check_cancel(&cancel)?;
        let input_rows = snapshot.data.row_count();
        let plan = crate::aggregation::plan(
            request,
            &snapshot.columns,
            &snapshot.data.schema(),
            input_rows,
        )?;
        if request.group_by.is_empty()
            && request
                .metrics
                .iter()
                .all(|metric| metric.op == crate::aggregation::AggregateOp::CountRows)
        {
            // The snapshot already contains the exact filtered row count.
            // Preserve the same Int64 result and aliases as COUNT(*).
            let schema = Arc::new(Schema::new_with_metadata(
                plan.labels
                    .iter()
                    .map(|label| Field::new(label, DataType::Int64, false))
                    .collect::<Vec<_>>(),
                snapshot.data.schema().metadata().clone(),
            ));
            let columns = crate::arrow_types::columns_from_schema(&schema);
            let count = Arc::new(Int64Array::from(vec![i64::try_from(input_rows)?]));
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![count as arrow::array::ArrayRef; plan.labels.len()],
            )?;
            let mut data = ArrowTable::new_with_schema(columns.clone(), vec![batch], schema)?;
            data.retain_memory(self.resources.reserve_display(data.memory_size())?);
            check_cancel(&cancel)?;
            progress.begin_phase(QueryPhase::FinishingAggregation);
            return Ok(crate::aggregation::AggregateResult {
                columns,
                data,
                group_by: Vec::new(),
                group_buckets: Vec::new(),
                input_rows,
                elapsed_ms: started.elapsed().as_millis(),
            });
        }
        let workers = self.ensure_query_runtime(&cancel)?;
        let runtime = self.runtime.as_ref().unwrap();
        let _entered = runtime.enter();
        let environment = crate::datafusion_session::memory_only_runtime(workspace_bytes(workers))?;
        let context = crate::datafusion_session::new_context(
            SessionConfig::new()
                .with_target_partitions(workers)
                .with_batch_size(32768),
            environment,
        );
        let schema = snapshot.data.schema();
        let aggregate_columns = || -> Result<Vec<usize>> {
            let mut projection = request
                .group_by
                .iter()
                .chain(
                    request
                        .metrics
                        .iter()
                        .filter_map(|metric| metric.field.as_ref()),
                )
                .map(|field| schema.index_of(field))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            projection.sort_unstable();
            projection.dedup();
            Ok(projection)
        };
        let projection = if snapshot.top_n.is_some() {
            // A Top N snapshot has an additional row restriction beyond the
            // predicate. Only its displayed rows belong to the aggregation.
            Some(aggregate_columns()?)
        } else {
            sparse_aggregate_projection(&snapshot, request)
        };
        let replay_predicate = projection.is_none();
        let (data, ranges, total) = if let Some(projection) = projection {
            let data = snapshot.data.project_columns(&projection)?;
            let rows = data.row_count();
            let partition_rows = rows
                .div_ceil(SPARSE_AGGREGATE_MAX_ROWS)
                .div_ceil(workers)
                .max(1)
                .saturating_mul(SPARSE_AGGREGATE_MAX_ROWS);
            let ranges = (0..rows.max(1))
                .step_by(partition_rows)
                .map(|start| {
                    AggregateRange::DisplayedRows(
                        start..start.saturating_add(partition_rows).min(rows),
                    )
                })
                .collect::<Vec<_>>();
            (data, ranges, rows)
        } else {
            // DataFusion pushes projection into the streaming scan. Lazily clone a
            // batch's handles as it is read, including any fields used by the filter.
            let data = snapshot.data.clone();
            let batches = data.raw_batches().len();
            let per_partition = batches.div_ceil(workers).max(1);
            let ranges = (0..batches.max(1))
                .step_by(per_partition)
                .map(|start| {
                    AggregateRange::SourceBatches(start..(start + per_partition).min(batches))
                })
                .collect::<Vec<_>>();
            (data, ranges, snapshot.data.base_row_count())
        };
        let scanned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        progress.advance_rows(QueryPhase::Aggregating, 0, total);
        let schema = data.schema();
        let partitions = ranges
            .into_iter()
            .map(|range| {
                Arc::new(AggregateInput {
                    data: data.clone(),
                    schema: schema.clone(),
                    range,
                    cancel: cancel.clone(),
                    progress: progress.clone(),
                    scanned: scanned.clone(),
                    total,
                }) as Arc<dyn PartitionStream>
            })
            .collect();
        let input = Arc::new(StreamingTable::try_new(schema, partitions)?);
        context.register_table("aggregation_input", input)?;
        let sql = plan.sql(if replay_predicate {
            &snapshot.filter
        } else {
            ""
        });
        let operation = cancel.clone();
        let resources = self.resources.clone();
        let progress = progress.clone();
        let task = runtime.spawn(async move {
            check_cancel(&operation)?;
            let frame = context.sql(&sql).await?;
            let raw_schema = frame.schema().as_arrow();
            ensure!(
                raw_schema.fields().len() == plan.labels.len() + 1,
                "Invalid aggregation result schema"
            );
            let schema = Arc::new(Schema::new_with_metadata(
                raw_schema
                    .fields()
                    .iter()
                    .zip(&plan.labels)
                    .map(|(field, label)| field.as_ref().clone().with_name(label))
                    .collect::<Vec<_>>(),
                raw_schema.metadata().clone(),
            ));
            let columns = crate::arrow_types::columns_from_schema(&schema);
            let mut stream = frame.execute_stream().await?;
            let mut output = Vec::new();
            let mut counted_rows = 0usize;
            while let Some(batch) = stream.try_next().await? {
                check_cancel(&operation)?;
                if counted_rows == 0 {
                    progress.begin_phase(QueryPhase::FinishingAggregation);
                }
                let counts = batch
                    .column(plan.labels.len())
                    .as_any()
                    .downcast_ref::<arrow::array::Int64Array>()
                    .context("Invalid aggregation row count")?;
                let count = arrow::compute::sum(counts).unwrap_or(0);
                counted_rows = counted_rows
                    .checked_add(usize::try_from(count)?)
                    .context("Aggregation row count overflow")?;
                let (_, mut arrays, rows) = batch.into_parts();
                arrays.truncate(plan.labels.len());
                output.push(RecordBatch::try_new_with_options(
                    schema.clone(),
                    arrays,
                    &arrow::record_batch::RecordBatchOptions::new().with_row_count(Some(rows)),
                )?);
            }
            check_cancel(&operation)?;
            ensure!(
                counted_rows == input_rows,
                "Aggregation input differs from the displayed snapshot"
            );
            let mut data = ArrowTable::new_with_schema(columns.clone(), output, schema)?;
            data.retain_memory(resources.reserve_display(data.memory_size())?);
            Ok::<_, anyhow::Error>((columns, data))
        });
        self.interrupt.set_abort(&cancel, task.abort_handle());
        let (columns, data) = match runtime.block_on(task) {
            Ok(result) => result.map_err(|error| {
                use datafusion::error::DataFusionError;
                let exhausted = error
                    .chain()
                    .filter_map(|cause| cause.downcast_ref::<DataFusionError>())
                    .any(|cause| {
                        cause.iter().any(|cause| {
                            matches!(cause.find_root(), DataFusionError::ResourcesExhausted(_))
                        })
                    });
                if exhausted {
                    error.context(
                        "Not enough query memory to calculate this summary. Reduce the number of rows or groups.",
                    )
                } else {
                    error
                }
            })?,
            Err(error) if error.is_cancelled() || cancel.load(Ordering::Acquire) => {
                bail!("Operation cancelled")
            }
            Err(error) => return Err(anyhow::anyhow!("DataFusion aggregation failed: {error}")),
        };
        check_cancel(&cancel)?;
        Ok(crate::aggregation::AggregateResult {
            columns,
            data,
            group_by: request.group_by.clone(),
            group_buckets: request.group_buckets.clone(),
            input_rows,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    #[allow(dead_code)]
    pub fn export_current(&self, path: &Path, format: ExportFormat) -> Result<()> {
        let snapshot = self
            .current_snapshot
            .as_deref()
            .context("Load a file before exporting.")?;
        self.export_result(snapshot, path, format)
    }
    #[allow(dead_code)]
    pub fn export_result(
        &self,
        snapshot_id: &str,
        path: &Path,
        format: ExportFormat,
    ) -> Result<()> {
        self.export_result_cancellable(snapshot_id, path, format, &AtomicBool::new(false))
    }
    #[allow(dead_code)]
    pub fn export_result_cancellable(
        &self,
        snapshot_id: &str,
        path: &Path,
        format: ExportFormat,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.export_result_columns_cancellable(snapshot_id, &[], path, format, cancel)
    }
    pub fn export_result_columns_cancellable(
        &self,
        snapshot_id: &str,
        selected: &[String],
        path: &Path,
        format: ExportFormat,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let snapshot = self
            .snapshots
            .get(snapshot_id)
            .context("The requested result snapshot is no longer available. Reload the file.")?;
        self.interrupt.run(cancel, |operation| {
            crate::export::export_snapshot(
                &snapshot.data,
                &snapshot.columns,
                selected,
                &self.info.paths,
                path,
                format,
                operation,
            )
        })
    }
    #[allow(dead_code)]
    pub fn retain_snapshot(&mut self, snapshot_id: &str) -> Result<()> {
        ensure!(
            self.snapshots.contains_key(snapshot_id),
            "Result snapshot is unavailable."
        );
        self.snapshots.retain(|key, _| key == snapshot_id);
        self.release_unused_window();
        self.current_snapshot = Some(snapshot_id.to_owned());
        Ok(())
    }
    pub fn discard_snapshot(&mut self, snapshot_id: &str) -> Result<()> {
        self.snapshots.remove(snapshot_id);
        self.release_unused_window();
        if self.current_snapshot.as_deref() == Some(snapshot_id) {
            self.current_snapshot = None;
        }
        Ok(())
    }

    fn release_unused_window(&mut self) {
        if self.window.as_ref().is_some_and(|window| {
            !self
                .snapshots
                .values()
                .any(|snapshot| snapshot.window_key == window.key)
        }) {
            self.window = None;
        }
        self.release_idle_query_resources();
    }

    fn release_idle_query_resources(&mut self) {
        // Retain workers only when a cached sort provider will reuse them.
        // Query operations join before reaching here, including on cancellation.
        if self
            .window
            .as_ref()
            .is_none_or(|window| window.context.is_none())
        {
            self.runtime = None;
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    Ok(())
}
fn unique_index_name(columns: &[ColumnInfo]) -> String {
    let mut name = "__pv_base_row_index".to_owned();
    while columns
        .iter()
        .any(|column| column.name.eq_ignore_ascii_case(&name))
    {
        name.push('_');
    }
    name
}
fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// This is an expression protocol generated by the typed editor, not an SQL
/// console. Only known pure scalar functions may inspect the registered window.
fn physical_visual_filter(
    predicate: &str,
    schema: arrow::datatypes::SchemaRef,
) -> Result<Arc<dyn datafusion::physical_expr::PhysicalExpr>> {
    let context = crate::datafusion_session::new_context(
        SessionConfig::new(),
        crate::datafusion_session::memory_only_runtime(workspace_bytes(1))?,
    );
    let schema = schema.try_into()?;
    let expr = context.parse_sql_expr(predicate, &schema)?;
    let state = context.state();
    let simplify_context = datafusion::logical_expr::simplify::SimplifyContext::builder()
        .with_schema(Arc::new(schema.clone()))
        .with_config_options(Arc::clone(state.config_options()))
        .with_query_execution_start_time(state.execution_props().query_execution_start_time)
        .build();
    // The physical planner coerces types but leaves visual filters' typed
    // arrow_cast literals for the scalar simplifier.
    let simplifier =
        datafusion::optimizer::simplify_expressions::ExprSimplifier::new(simplify_context);
    let expr = simplifier.simplify(simplifier.coerce(expr, &schema)?)?;
    Ok(state.create_physical_expr(expr, &schema)?)
}

fn validated_filter(value: &str) -> Result<String> {
    let mut expression = value.trim();
    if expression
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("WHERE"))
        && expression
            .get(5..)
            .is_some_and(|tail| tail.starts_with(char::is_whitespace))
    {
        expression = expression[5..].trim();
    }
    ensure!(
        !expression.contains('\0'),
        "NUL is not allowed in filter expressions."
    );
    let mut chars = expression.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '"' => {
                let mut closed = false;
                while let Some(next) = chars.next() {
                    if next == ch {
                        if chars.peek() == Some(&ch) {
                            chars.next();
                        } else {
                            closed = true;
                            break;
                        }
                    }
                }
                ensure!(closed, "Unterminated quoted filter value or field.");
            }
            ';' => bail!("Only one filter expression is allowed."),
            '-' if chars.peek() == Some(&'-') => {
                bail!("Comments are not allowed in filter expressions.")
            }
            '/' if chars.peek() == Some(&'*') => {
                bail!("Comments are not allowed in filter expressions.")
            }
            ch if ch.is_alphabetic() || ch == '_' => {
                let mut word = String::from(ch);
                while chars
                    .peek()
                    .is_some_and(|ch| ch.is_alphanumeric() || *ch == '_')
                {
                    word.push(chars.next().unwrap());
                }
                let keyword = word.to_ascii_uppercase();
                ensure!(
                    !matches!(
                        keyword.as_str(),
                        "SELECT"
                            | "FROM"
                            | "JOIN"
                            | "UNION"
                            | "WITH"
                            | "EXISTS"
                            | "INSERT"
                            | "UPDATE"
                            | "DELETE"
                            | "CREATE"
                            | "DROP"
                            | "ALTER"
                            | "COPY"
                            | "ATTACH"
                            | "DETACH"
                            | "INSTALL"
                            | "LOAD"
                            | "PRAGMA"
                            | "CALL"
                            | "EXECUTE"
                            | "PREPARE"
                            | "SET"
                            | "RESET"
                            | "GRANT"
                            | "REVOKE"
                            | "VACUUM"
                            | "EXPLAIN"
                            | "SHOW"
                            | "TRUNCATE"
                    ),
                    "Only scalar filter expressions are allowed."
                );
                if chars.clone().find(|ch| !ch.is_whitespace()) == Some('(') {
                    ensure!(
                        matches!(
                            keyword.as_str(),
                            "CAST"
                                | "TRY_CAST"
                                | "ARROW_CAST"
                                | "GET_FIELD"
                                | "ARRAY_HAS"
                                | "ARRAY_LENGTH"
                                | "STRPOS"
                                | "LOWER"
                                | "UPPER"
                                | "STARTS_WITH"
                                | "ENDS_WITH"
                                | "CONTAINS"
                                | "LENGTH"
                                | "CHAR_LENGTH"
                                | "TRIM"
                                | "COALESCE"
                                | "NULLIF"
                                | "IN"
                                | "NOT"
                                | "AND"
                                | "OR"
                                | "AS"
                                | "DECIMAL"
                                | "NUMERIC"
                                | "VARCHAR"
                                | "CHAR"
                                | "TIMESTAMP"
                                | "TIME"
                        ),
                        "Function {word:?} is not available in visual filters."
                    );
                }
            }
            _ => {}
        }
    }
    Ok(expression.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::{
        array::{
            BinaryArray, BooleanArray, Decimal128Array, Int32Array, Int64Array, ListArray,
            StringArray, StructArray, TimestampNanosecondArray,
        },
        buffer::{NullBuffer, OffsetBuffer},
        datatypes::Int64Type,
    };
    use parquet::{arrow::ArrowWriter, file::properties::WriterProperties};
    use std::{
        fs::{self, File},
        sync::Barrier,
    };

    fn write_fixture(path: &Path, batch: &RecordBatch, row_group_rows: usize) -> Result<()> {
        let properties = WriterProperties::builder()
            .set_max_row_group_row_count(Some(row_group_rows))
            .build();
        let mut writer =
            ArrowWriter::try_new(File::create(path)?, batch.schema(), Some(properties))?;
        writer.write(batch)?;
        writer.close()?;
        Ok(())
    }

    #[test]
    fn resuming_text_browsing_keeps_the_successful_first_page_after_indexing() -> Result<()> {
        use std::fmt::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("wide.csv");
        let payload = "x".repeat(16 * 1024);
        let mut text = String::from("id,score,payload\n");
        for id in 0..80 {
            let score = if id == 20 { "unknown" } else { "42" };
            writeln!(text, "{id},{score},{payload}")?;
        }
        fs::write(&path, text)?;
        let cancel = AtomicBool::new(false);
        let sample = csv_source::CsvProbe::read_for_browse(&path, &cancel)?;
        assert!(sample.sampled_records < 20);
        let mut engine = Engine::open_prepared_csv(&path, sample.prepared(), &cancel)?;
        let (_, _, first) = engine.open_csv_view(&cancel)?;
        let fixed = engine.query(&QueryRequest {
            offset: 70,
            limit: 10,
            selected_columns: vec!["id".into()],
            ..Default::default()
        })?;
        assert_eq!(fixed.source_row_number(9), Some(80));
        let (_, _, restored) = engine.open_csv_view(&cancel)?;
        assert_eq!(restored.row_count(), first.row_count());
        assert_eq!(restored.columns, first.columns);
        Ok(())
    }

    #[test]
    fn text_browsing_and_fixed_queries_keep_independent_typed_snapshots() -> Result<()> {
        use std::fmt::Write;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("pages.tsv");
        let mut source = String::from("id\tcode\n");
        for row in 0..3000 {
            writeln!(source, "{row}\t{row:05}")?;
        }
        fs::write(&path, source)?;
        let cancel = AtomicBool::new(false);
        let sample = csv_source::CsvProbe::read_for_browse(&path, &cancel)?;
        let mut engine = Engine::open_prepared_csv(&path, sample.prepared(), &cancel)?;
        let (_, _, first) = engine.open_csv_view(&cancel)?;
        assert_eq!(first.row_count(), 1000);
        let selected = engine.query(&QueryRequest {
            offset: 2500,
            limit: 2,
            selected_columns: vec!["code".into()],
            ..Default::default()
        })?;
        assert_eq!(selected.cell(1, 0).text, "02501");
        let (index, _, reopened) = engine.open_csv_view(&cancel)?;
        assert_eq!(reopened.columns, first.columns);
        assert_eq!(reopened.cell(0, 1).text, "00000");
        let page = index.read_page(2500, 2, 1024, &engine.resources, &cancel)?;
        assert_eq!(page.source_row_number(1), Some(2502));
        assert_eq!(page.display_text(1, 1), selected.cell(1, 0).text);
        let export = directory.path().join("selection.csv");
        engine.export_result(&selected.snapshot_id, &export, ExportFormat::Csv)?;
        assert_eq!(
            fs::read_to_string(export)?.lines().collect::<Vec<_>>(),
            ["code", "02500", "02501"]
        );
        Ok(())
    }

    #[test]
    fn opens_delimited_text_through_the_normal_file_entry_point() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("measurements.TSV");
        fs::write(&source, "id\tvalue\n2147483648\t3\n")?;
        let mut engine = Engine::open(&[source])?;
        let result = engine.query(&QueryRequest::default())?;
        assert_eq!(result.columns[0].data_type, "INT64");
        assert_eq!(result.cell(0, 0).text, "2147483648");
        assert_eq!(result.cell(0, 1).text, "3");
        Ok(())
    }

    #[test]
    fn csv_whole_source_filter_and_summary_share_source_rows_and_exact_values() -> Result<()> {
        use crate::aggregation::{AggregateMetric, AggregateOp, AggregateRequest};

        let directory = tempfile::tempdir()?;
        let source = directory.path().join("sales.CSV");
        fs::write(
            &source,
            "region,amount,include\nEast,1.15,true\nWest,2.53,false\nEast,2.05,true\nEast,,true\n",
        )?;
        let cancel = AtomicBool::new(false);
        let mut engine = Engine::open_cancellable(&[source], &cancel)?;
        let request = QueryRequest {
            selected_columns: vec!["region".into(), "amount".into()],
            whole_source: true,
            filter: "\"include\" = true".into(),
            ..Default::default()
        };
        let result = engine.query(&request)?;
        assert_eq!(result.total_rows, 4);
        assert!(result.total_rows_known);
        assert_eq!(result.filtered_rows, Some(3));
        assert_eq!(
            (0..result.row_count())
                .map(|row| result.source_row_number(row))
                .collect::<Vec<_>>(),
            [Some(1), Some(3), Some(4)]
        );
        let summary = engine.aggregate_snapshot_cancellable(
            &result.snapshot_id,
            &AggregateRequest {
                group_by: vec!["region".into()],
                metrics: [AggregateOp::Sum, AggregateOp::Count, AggregateOp::Mean]
                    .into_iter()
                    .map(|op| AggregateMetric {
                        field: Some("amount".into()),
                        op,
                    })
                    .collect(),
                ..Default::default()
            },
            &cancel,
        )?;
        assert_eq!(summary.data.row_count(), 1);
        assert_eq!(summary.data.display_text(0, 0), "East");
        assert_eq!(summary.data.display_text(0, 1), "3.20");
        assert_eq!(summary.data.display_text(0, 2), "2");
        assert_eq!(summary.data.display_text(0, 3).trim_end_matches('0'), "1.6");
        Ok(())
    }

    #[test]
    fn legacy_interval_engine_sort_views_and_exports_keep_original_components() -> Result<()> {
        use arrow::array::{FixedSizeBinaryBuilder, MapArray};
        use serde_json::{Value, json};

        let directory = tempfile::tempdir()?;
        let source = directory.path().join("interval.parquet");
        let values = [
            Some((1i32, 2i32, 3i32)),
            Some((0, 40, 0)),
            Some((0, 0, 256)),
            Some((0, 0, 1)),
            Some((1, 0, 0)),
            Some((0, 30, 0)),
            None,
            Some((-1, -2, -3)),
        ];
        let mut builder = FixedSizeBinaryBuilder::with_capacity(values.len(), 12);
        for value in values {
            if let Some((months, days, milliseconds)) = value {
                let mut bytes = [0u8; 12];
                bytes[..4].copy_from_slice(&months.to_le_bytes());
                bytes[4..8].copy_from_slice(&days.to_le_bytes());
                bytes[8..].copy_from_slice(&milliseconds.to_le_bytes());
                builder.append_value(bytes)?;
            } else {
                builder.append_null();
            }
        }
        let span = Arc::new(builder.finish()) as ArrayRef;
        let interval = Field::new("span", DataType::FixedSizeBinary(12), true).with_metadata(
            [(
                "parquetviewer.logical_type".into(),
                "INTERVAL_MONTH_DAY_MILLISECOND_LE".into(),
            )]
            .into_iter()
            .collect(),
        );
        let validity = NullBuffer::from(values.iter().map(Option::is_some).collect::<Vec<_>>());
        let record = Arc::new(StructArray::try_new(
            vec![Arc::new(interval.clone())].into(),
            vec![span.clone()],
            Some(validity.clone()),
        )?) as ArrayRef;
        let items = Arc::new(ListArray::try_new(
            Arc::new(interval.clone().with_name("element")),
            OffsetBuffer::new((0i32..=8).collect::<Vec<_>>().into()),
            span.clone(),
            Some(validity.clone()),
        )?) as ArrayRef;
        let entries = StructArray::try_new(
            vec![
                Arc::new(Field::new("key", DataType::Utf8, false)),
                Arc::new(interval.clone().with_name("value")),
            ]
            .into(),
            vec![Arc::new(StringArray::from(vec!["span"; 8])), span.clone()],
            None,
        )?;
        let mapping = Arc::new(MapArray::try_new(
            Arc::new(Field::new("entries", entries.data_type().clone(), false)),
            OffsetBuffer::new((0i32..=8).collect::<Vec<_>>().into()),
            entries,
            Some(validity),
            false,
        )?) as ArrayRef;
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            interval,
            Field::new("record", record.data_type().clone(), true),
            Field::new("items", items.data_type().clone(), true),
            Field::new("mapping", mapping.data_type().clone(), true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..8)),
                span,
                record,
                items,
                mapping,
            ],
        )?;
        let properties = WriterProperties::builder()
            .set_max_row_group_row_count(Some(2))
            .build();
        let options = crate::parquet_compat::writer_options(&schema, properties)?
            .with_skip_arrow_metadata(true);
        let mut writer =
            ArrowWriter::try_new_with_options(File::create_new(&source)?, schema, options)?;
        writer.write(&batch)?;
        writer.close()?;

        let mut engine = Engine::open(&[source])?;
        assert_eq!(
            engine
                .info
                .columns
                .iter()
                .map(|column| column.data_type.as_str())
                .collect::<Vec<_>>(),
            [
                "INT64",
                "INTERVAL",
                "STRUCT(\"span\" INTERVAL)",
                "INTERVAL[]",
                "MAP(VARCHAR, INTERVAL)"
            ],
        );
        let first_filter = engine.query(&QueryRequest {
            offset: 1,
            limit: 6,
            filter: "id >= 3".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&first_filter), [3, 4, 5, 6]);
        let base = engine.window.as_ref().unwrap().base.clone();
        assert!(
            engine
                .window
                .as_ref()
                .unwrap()
                .interval_sort_targets
                .is_empty()
        );
        // A filter-only query must not build any compatibility sort keys.
        assert!(engine.window.as_ref().unwrap().sort_names.is_empty());
        let mut snapshot = String::new();
        for key in ["span", "record", "items", "mapping"] {
            let result = engine.query(&QueryRequest {
                offset: 1,
                limit: 6,
                sort: Some((key.into(), false)),
                ..Default::default()
            })?;
            assert_eq!(engine.window.as_ref().unwrap().interval_sort_targets, [key]);
            for other in ["span", "record", "items"] {
                assert_eq!(
                    engine
                        .window
                        .as_ref()
                        .unwrap()
                        .sort_names
                        .contains_key(other),
                    other == key
                );
            }
            assert!(result.timings.window_cache_hit);
            assert!(Arc::ptr_eq(
                base.raw_batches()[0].column(1),
                result.data.raw_batches()[0].column(1)
            ));
            assert_eq!(ids(&result), [3, 2, 4, 5, 1, 6], "sort by {key}");
            assert_eq!(
                (0..6)
                    .map(|row| result.data.source_row_number(row))
                    .collect::<Vec<_>>(),
                [Some(4), Some(3), Some(5), Some(6), Some(2), Some(7)],
            );
            assert_eq!(result.data.cell(0, 1).text, "00:00:00.001");
            assert_eq!(result.data.cell(2, 1).text, "1 month");
            assert_eq!(result.data.cell(3, 1).text, "30 days");
            assert!(result.data.is_null(5, 1));
            assert!(result.data.is_null(5, 2));
            assert!(result.data.is_null(5, 3));
            assert!(result.data.is_null(5, 4));
            snapshot = result.snapshot_id;
        }
        let cleared = engine.query(&QueryRequest {
            offset: 1,
            limit: 6,
            ..Default::default()
        })?;
        assert_eq!(ids(&cleared), [1, 2, 3, 4, 5, 6]);
        assert!(
            engine
                .window
                .as_ref()
                .unwrap()
                .interval_sort_targets
                .is_empty()
        );
        assert!(engine.window.as_ref().unwrap().context.is_none());
        assert!(Arc::ptr_eq(
            base.raw_batches()[0].column(1),
            cleared.data.raw_batches()[0].column(1)
        ));
        let filtered = engine.query(&QueryRequest {
            offset: 1,
            limit: 6,
            filter: "\"span\" IS NOT NULL".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&filtered), [1, 2, 3, 4, 5]);
        assert!(
            engine
                .window
                .as_ref()
                .unwrap()
                .interval_sort_targets
                .is_empty()
        );
        assert!(engine.window.as_ref().unwrap().sort_names.is_empty());
        let by_id = engine.query(&QueryRequest {
            offset: 1,
            limit: 6,
            sort: Some(("id".into(), true)),
            ..Default::default()
        })?;
        assert_eq!(ids(&by_id), [6, 5, 4, 3, 2, 1]);
        assert_eq!(
            (0..by_id.row_count())
                .map(|row| by_id.source_row_number(row))
                .collect::<Vec<_>>(),
            [Some(7), Some(6), Some(5), Some(4), Some(3), Some(2)]
        );
        assert!(
            engine
                .window
                .as_ref()
                .unwrap()
                .interval_sort_targets
                .is_empty()
        );
        assert!(Arc::ptr_eq(
            base.raw_batches()[0].column(1),
            by_id.data.raw_batches()[0].column(1)
        ));

        let json_path = directory.path().join("interval.json");
        engine.export_result(&snapshot, &json_path, ExportFormat::Json)?;
        let actual: Value = serde_json::from_reader(File::open(&json_path)?)?;
        assert_eq!(actual[2]["span"], json!("1 month"));
        assert_eq!(actual[3]["span"], json!("30 days"));
        for row in actual.as_array().unwrap() {
            if row["span"].is_null() {
                assert!(row["record"].is_null());
                assert!(row["items"].is_null());
                assert!(row["mapping"].is_null());
            } else {
                assert_eq!(row["span"], row["record"]["span"]);
                assert_eq!(row["span"], row["items"][0]);
                assert_eq!(row["span"], row["mapping"]["span"]);
            }
        }
        let parquet_path = directory.path().join("interval-export.parquet");
        engine.export_result(&snapshot, &parquet_path, ExportFormat::Parquet)?;
        let mut reopened = Engine::open(&[parquet_path])?;
        let result = reopened.query(&QueryRequest::default())?;
        assert_eq!(ids(&result), [3, 2, 4, 5, 1, 6]);
        assert_eq!(result.columns, engine.info.columns);
        let roundtrip_json = directory.path().join("interval-roundtrip.json");
        reopened.export_result(&result.snapshot_id, &roundtrip_json, ExportFormat::Json)?;
        let roundtrip: Value = serde_json::from_reader(File::open(roundtrip_json)?)?;
        assert_eq!(roundtrip, actual);
        Ok(())
    }

    fn fixture(path: &Path, start: i64, rows: usize) -> Result<()> {
        let ids = Arc::new(Int64Array::from_iter_values(
            start..start + i64::try_from(rows)?,
        ));
        let titles = Arc::new(StringArray::from_iter_values(
            (0..rows).map(|row| format!("row {} O'Brien 上海", start + row as i64)),
        ));
        let wide = Arc::new(StringArray::from_iter_values(
            (0..rows).map(|row| format!("{}-{row}", "x".repeat(1024))),
        ));
        let lists = Arc::new(ListArray::from_iter_primitive::<Int64Type, _, _>(
            (0..rows).map(|row| Some(vec![Some(start + row as i64), None])),
        ));
        let nested = Arc::new(StructArray::from(vec![
            (
                Arc::new(Field::new("x", DataType::Int64, false)),
                ids.clone() as ArrayRef,
            ),
            (
                Arc::new(Field::new("items", lists.data_type().clone(), true)),
                lists as ArrayRef,
            ),
        ]));
        let amounts = Arc::new(
            Decimal128Array::from_iter_values(
                (0..rows).map(|row| 9007199254740993123456789012345678i128 + row as i128),
            )
            .with_precision_and_scale(38, 18)?,
        );
        let binary = Arc::new(BinaryArray::from_iter_values(
            (0..rows).map(|_| [0u8, 1, 255]),
        ));
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("title", DataType::Utf8, false),
            Field::new("payload", DataType::Utf8, false),
            Field::new("nested", nested.data_type().clone(), true),
            Field::new("amount", DataType::Decimal128(38, 18), false),
            Field::new("bytes", DataType::Binary, false),
        ]));
        let batch = RecordBatch::try_new(schema, vec![ids, titles, wide, nested, amounts, binary])?;
        write_fixture(path, &batch, 4)
    }

    fn ids(result: &QueryResult) -> Vec<i64> {
        let column = result
            .columns
            .iter()
            .position(|column| column.name == "id")
            .unwrap();
        (0..result.row_count())
            .map(|row| result.cell(row, column).text.parse().unwrap())
            .collect()
    }

    #[test]
    fn streaming_identity_is_recreated_per_scan_and_not_retained_by_the_provider() -> Result<()> {
        let source = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
            vec![Arc::new(Int64Array::from(vec![10, 11, 12]))],
        )?;
        let base = ArrowTable::new(
            crate::arrow_types::columns_from_schema(&source.schema()),
            vec![source.clone()],
        )?;
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            for (index_width, data_type, first) in [
                (
                    RowIndexWidth::Compact,
                    DataType::UInt32,
                    u64::from(u32::MAX) - 2,
                ),
                (
                    RowIndexWidth::Wide,
                    DataType::UInt64,
                    u64::from(u32::MAX) + 100,
                ),
            ] {
                let partition = IndexedPartition {
                    schema: Arc::new(Schema::new(vec![
                        Field::new("id", DataType::Int64, false),
                        Field::new("identity", data_type.clone(), false),
                    ])),
                    base: base.clone(),
                    batches: 0..1,
                    offsets: Arc::new(vec![first]),
                    adapted_columns: Arc::new(Vec::new()),
                    index_width,
                };
                for _ in 0..2 {
                    let mut stream = partition.execute(Arc::new(TaskContext::default()));
                    let output = stream.try_next().await?.unwrap();
                    assert!(Arc::ptr_eq(output.column(0), source.column(0)));
                    assert_eq!(output.column(1).data_type(), &data_type);
                    for row in 0..3 {
                        assert_eq!(
                            arrow::util::display::array_value_to_string(
                                output.column(1).as_ref(),
                                row
                            )?,
                            (first + row as u64).to_string(),
                        );
                    }
                    let identity = Arc::downgrade(output.column(1));
                    drop(output);
                    assert!(stream.try_next().await?.is_none());
                    assert!(
                        identity.upgrade().is_none(),
                        "The live provider must not retain generated identity arrays"
                    );
                }
            }
            Ok::<_, anyhow::Error>(())
        })
    }

    #[test]
    fn ungrouped_count_rows_reads_the_exact_snapshot_without_starting_datafusion() -> Result<()> {
        use crate::aggregation::{AggregateMetric, AggregateOp, AggregateRequest};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("snapshot-count.parquet");
        fixture(&path, 0, 12)?;
        let mut engine = Engine::open(&[path])?;
        let request = AggregateRequest {
            group_by: Vec::new(),
            group_buckets: Vec::new(),
            metrics: vec![
                AggregateMetric {
                    field: None,
                    op: AggregateOp::CountRows
                };
                2
            ],
        };
        let cancel = AtomicBool::new(false);
        let loaded = engine.query(&QueryRequest {
            offset: 2,
            limit: 8,
            ..Default::default()
        })?;
        let counted =
            engine.aggregate_snapshot_cancellable(&loaded.snapshot_id, &request, &cancel)?;
        assert!(engine.runtime.is_none());
        assert_eq!(counted.input_rows, 8);
        assert_eq!(counted.data.row_count(), 1);
        assert_eq!(counted.data.cell(0, 0).text, "8");
        assert_eq!(counted.columns[0].data_type, "INT64");
        assert!(!counted.columns[0].nullable);
        assert_eq!(counted.columns[1].name, "Count rows (2)");
        assert_eq!(counted.data.cell(0, 1).text, "8");
        for (filter, expected) in [("id >= 8", 2), ("id < 0", 0)] {
            let result = engine.query(&QueryRequest {
                offset: 2,
                limit: 8,
                filter: filter.into(),
                ..Default::default()
            })?;
            let counted =
                engine.aggregate_snapshot_cancellable(&result.snapshot_id, &request, &cancel)?;
            assert_eq!(counted.input_rows, expected);
            assert_eq!(counted.data.row_count(), 1);
            assert_eq!(counted.data.cell(0, 0).text, expected.to_string());
        }
        Ok(())
    }

    #[test]
    fn sparse_aggregate_reuses_sorted_selection_with_exact_sum_and_null_group() -> Result<()> {
        use crate::aggregation::{AggregateMetric, AggregateOp, AggregateRequest};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sparse-groups.parquet");
        let rows = 120_000i64;
        let batch = RecordBatch::try_from_iter([
            (
                "id",
                Arc::new(Int64Array::from_iter_values(0..rows)) as ArrayRef,
            ),
            (
                "category",
                Arc::new(StringArray::from_iter(
                    (0..rows).map(|id| (id % 2 == 1).then_some("odd")),
                )) as ArrayRef,
            ),
            (
                "unused",
                Arc::new(Int64Array::from_iter_values(0..rows)) as ArrayRef,
            ),
        ])?;
        write_fixture(&path, &batch, 16_384)?;
        let mut engine = Engine::open(std::slice::from_ref(&path))?;
        let detail = engine.query(&QueryRequest {
            limit: rows as usize,
            filter: "id IN (11,50000,100000)".into(),
            sort: Some(("id".into(), true)),
            ..Default::default()
        })?;
        assert_eq!(ids(&detail), [100000, 50000, 11]);
        let request = AggregateRequest {
            group_by: vec!["category".into()],
            group_buckets: Vec::new(),
            metrics: vec![
                AggregateMetric {
                    field: None,
                    op: AggregateOp::CountRows,
                },
                AggregateMetric {
                    field: Some("id".into()),
                    op: AggregateOp::Sum,
                },
            ],
        };
        let snapshot = &engine.snapshots[&detail.snapshot_id];
        assert_eq!(
            sparse_aggregate_projection(snapshot, &request),
            Some(vec![1, 0])
        );
        fs::remove_file(path)?;
        let result = engine.aggregate_snapshot_cancellable(
            &detail.snapshot_id,
            &request,
            &AtomicBool::new(false),
        )?;
        assert_eq!(result.input_rows, 3);
        assert_eq!(result.data.row_count(), 2);
        assert_eq!(result.data.cell(0, 0).text, "odd");
        assert!(result.data.is_null(1, 0));
        assert_eq!(result.data.cell(0, 1).text, "1");
        assert_eq!(result.data.cell(1, 1).text, "2");
        assert_eq!(result.data.cell(0, 2).text, "11");
        assert_eq!(result.data.cell(1, 2).text, "150000");
        assert_eq!(
            result.data.schema().field(2).data_type(),
            &DataType::Decimal128(38, 0)
        );
        assert_eq!(detail.source_row_number(0), Some(100001));
        assert_eq!(ids(&detail), [100000, 50000, 11]);
        Ok(())
    }

    #[test]
    fn sparse_aggregate_rejects_large_text_input_even_for_one_match() -> Result<()> {
        use crate::aggregation::{AggregateMetric, AggregateOp, AggregateRequest};
        use arrow::array::UInt32Array;

        let text = "x".repeat(SPARSE_AGGREGATE_MAX_TEXT_BYTES);
        let rows = 100_000;
        let values =
            StringArray::from_iter((0..rows).map(|row| (row == 0).then_some(text.as_str())));
        let batch = RecordBatch::try_from_iter([("text", Arc::new(values) as ArrayRef)])?;
        let columns = crate::arrow_types::columns_from_schema(&batch.schema());
        let base = ArrowTable::new(columns.clone(), vec![batch])?;
        let snapshot = Snapshot {
            window_key: WindowKey {
                columns: vec!["text".into()],
                offset: 0,
                limit: rows,
                source_predicate: None,
            },
            columns,
            data: base.with_row_index_batches(vec![UInt32Array::from(vec![0])])?,
            filter: "text IS NOT NULL".into(),
            sort_keys: Vec::new(),
            top_n: None,
        };
        let request = AggregateRequest {
            group_by: vec!["text".into()],
            group_buckets: Vec::new(),
            metrics: vec![AggregateMetric {
                field: None,
                op: AggregateOp::CountRows,
            }],
        };
        assert!(sparse_aggregate_projection(&snapshot, &request).is_none());
        Ok(())
    }

    #[test]
    fn streaming_identity_keeps_ranged_source_order_sparse_nulls_and_stable_sort() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("partitioned-identity.parquet");
        let rows = 100_003i64;
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("key", DataType::Int32, true),
            ])),
            vec![
                Arc::new(Int64Array::from_iter_values(0..rows)),
                Arc::new(Int32Array::from_iter(
                    (0..rows).map(|id| (id % 13 != 0).then_some((id % 7) as i32)),
                )),
            ],
        )?;
        write_fixture(&path, &batch, 16_384)?;
        let mut engine = Engine::open(&[path])?;
        let request = QueryRequest {
            offset: 13,
            limit: 70_001,
            filter: "id >= 0".into(),
            ..Default::default()
        };
        let broad = engine.query(&request)?;
        assert!(broad.data.raw_batches().len() > 1);
        assert_eq!(ids(&broad), (13..70_014).collect::<Vec<_>>());
        assert!(engine.runtime.is_none());
        fs::remove_file(&engine.info.paths[0])?;
        let sparse = engine.query(&QueryRequest {
            filter: "id IN (13,32768,65536,70013)".into(),
            ..request.clone()
        })?;
        assert_eq!(ids(&sparse), [13, 32768, 65536, 70013]);
        for row in 0..sparse.row_count() {
            assert_eq!(
                sparse.source_row_number(row),
                Some(sparse.cell(row, 0).text.parse::<usize>()? + 1)
            );
        }
        let nulls = engine.query(&QueryRequest {
            filter: "\"key\" IS NULL AND id >= 32760 AND id <= 65550".into(),
            ..request.clone()
        })?;
        assert_eq!(
            ids(&nulls),
            (32_760..=65_550)
                .filter(|id| id % 13 == 0)
                .collect::<Vec<_>>()
        );
        let sorted = engine.query(&QueryRequest {
            sort: Some(("key".into(), true)),
            ..request
        })?;
        let mut expected = (13..70_014).filter(|id| id % 13 != 0).collect::<Vec<_>>();
        expected.sort_by_key(|id| (std::cmp::Reverse(id % 7), *id));
        expected.extend((13..70_014).filter(|id| id % 13 == 0));
        assert_eq!(ids(&sorted), expected);
        let sort_context = &engine.window.as_ref().unwrap().context.as_ref().unwrap().0;
        assert!(!sort_context.runtime_env().disk_manager.tmp_files_enabled());
        for row in [0, 16_383, 32_768, 65_536, sorted.row_count() - 1] {
            assert_eq!(
                sorted.source_row_number(row),
                Some(expected[row] as usize + 1)
            );
        }
        Ok(())
    }

    #[test]
    fn paging_precedes_filter_sort_and_views_keep_original_rows_and_buffers() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("O'Brien.parquet");
        fixture(&source, 0, 12)?;
        let mut engine = Engine::open(&[source])?;
        let progress = QueryProgress::default();
        let result = engine.query_cancellable_with_progress(
            &QueryRequest {
                offset: 2,
                limit: 7,
                filter: "\"id\" >= 4".into(),
                sort: Some(("id".into(), true)),
                ..Default::default()
            },
            &AtomicBool::new(false),
            &progress,
        )?;
        assert_eq!(ids(&result), [8, 7, 6, 5, 4]);
        assert_eq!(result.loaded_rows, 7);
        assert_eq!(result.total_rows, 12);
        assert_eq!(
            (0..result.row_count())
                .map(|row| result.source_row_number(row))
                .collect::<Vec<_>>(),
            vec![Some(9), Some(8), Some(7), Some(6), Some(5)]
        );
        assert!(Arc::ptr_eq(
            engine.window.as_ref().unwrap().base.raw_batches()[0].column(2),
            result.data.raw_batches()[0].column(2)
        ));
        assert_eq!(result.cell(0, 3).json.as_ref().unwrap()["x"], 8);
        assert_eq!(
            result.cell(0, 4).text,
            "9007199254740993.123456789012345686"
        );
        assert_eq!(result.binary(0, 5), Some(&[0, 1, 255][..]));
        assert_eq!(progress.snapshot().phase, QueryPhase::Complete);
        assert_eq!(progress.snapshot().fraction, Some(1.));
        assert!(!result.timings.window_cache_hit);
        assert_eq!(result.timings.snapshot_ms, 0.);
        Ok(())
    }

    #[test]
    fn loaded_filter_keeps_order_and_null_semantics_without_a_query_workspace() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("filter.parquet");
        let batch = RecordBatch::try_from_iter([(
            "id",
            Arc::new(Int64Array::from_iter(
                (0..18).map(|id| (id != 8).then_some(id)),
            )) as ArrayRef,
        )])?;
        write_fixture(&path, &batch, 6)?;
        let mut engine = Engine::open(&[path])?;
        engine.source.set_test_batch_rows(4);
        let mut request = QueryRequest {
            offset: 2,
            limit: 12,
            ..Default::default()
        };
        let original = engine.query(&request)?;
        assert_eq!(original.filtered_rows, None);
        request.filter = "\"id\" >= ARROW_CAST('3','Int64')".into();
        let filtered = engine.query(&request)?;
        assert_eq!(ids(&filtered), [3, 4, 5, 6, 7, 9, 10, 11, 12, 13]);
        assert_eq!(filtered.filtered_rows, Some(10));
        assert_eq!(
            (0..filtered.row_count())
                .map(|row| filtered.source_row_number(row).unwrap())
                .collect::<Vec<_>>(),
            [4, 5, 6, 7, 8, 10, 11, 12, 13, 14]
        );
        assert!(Arc::ptr_eq(
            original.data.raw_batches()[0].column(0),
            filtered.data.raw_batches()[0].column(0),
        ));
        request.filter = "\"id\" IS NULL OR \"id\" >= 0".into();
        let all = engine.query(&request)?;
        assert_eq!(all.row_count(), original.row_count());
        assert_eq!(all.filtered_rows, None);
        assert!(!all.data.has_row_indices());
        request.filter = "NULL".into();
        let empty = engine.query(&request)?;
        assert_eq!(empty.row_count(), 0);
        assert_eq!(empty.filtered_rows, Some(0));
        assert!(engine.runtime.is_none());
        Ok(())
    }

    #[test]
    fn contiguous_loaded_filter_does_not_allocate_per_row_indices() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("contiguous.parquet");
        let field = Arc::new(Int64Array::from_iter_values(0..240_000));
        let batch = RecordBatch::try_from_iter([("id", field as ArrayRef)])?;
        write_fixture(&path, &batch, 60_000)?;
        let mut engine = Engine::open(&[path])?;
        let mut request = QueryRequest {
            limit: 240_000,
            ..Default::default()
        };
        let original = engine.query(&request)?;
        request.filter = "id >= 15000 AND id < 190000".into();
        let first = engine.query(&request)?;
        assert_eq!(first.filtered_rows, Some(175_000));
        assert_eq!(first.source_row_number(0), Some(15_001));
        assert_eq!(first.source_row_number(174_999), Some(190_000));
        assert_eq!(first.source_row_number(175_000), None);
        assert!(first.data.memory_size() - original.data.memory_size() < 4096);
        assert!(Arc::ptr_eq(
            first.data.raw_batches()[0].column(0),
            original.data.raw_batches()[0].column(0)
        ));
        let cancel = AtomicBool::new(false);
        let exported = first
            .data
            .export_batches(512, &cancel)
            .next()
            .context("Expected a filtered export batch")??;
        let (source_batch, _) = original
            .data
            .raw_batches()
            .iter()
            .enumerate()
            .find(|(i, batch)| {
                let start = original.data.raw_batch_start(*i);
                (start..start + batch.num_rows()).contains(&15_000)
            })
            .context("Expected the first matched source batch")?;
        let original_values = original.data.raw_batches()[source_batch]
            .column(0)
            .to_data();
        let slice_values = exported.column(0).to_data();
        assert_eq!(
            original_values.buffers()[0].data_ptr(),
            slice_values.buffers()[0].data_ptr()
        );

        // The first chunk is sparse, the subsequent chunks are contiguous.
        request.filter = "id IN (3,12) OR id >= 70000".into();
        let mixed = engine.query(&request)?;
        assert_eq!(mixed.source_row_number(1), Some(13));
        assert_eq!(mixed.source_row_number(2), Some(70_001));
        let first_three = mixed.data.materialize_range(0..3)?;
        assert_eq!(
            first_three
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values(),
            &[3, 12, 70_000]
        );
        Ok(())
    }

    #[test]
    fn whole_source_visual_filter_matches_ram_and_preserves_snapshot_across_scope_changes()
    -> Result<()> {
        use crate::{
            aggregation::{AggregateMetric, AggregateOp, AggregateRequest},
            filtering::{FilterCondition, FilterGroup, FilterOperator, MatchMode},
        };

        let directory = tempfile::tempdir()?;
        let first = directory.path().join("a.parquet");
        let second = directory.path().join("b.parquet");
        fixture(&first, 0, 12)?;
        fixture(&second, 12, 12)?;
        let mut engine = Engine::open(&[second, first])?;
        let filter = FilterGroup {
            mode: MatchMode::All,
            conditions: vec![
                FilterCondition {
                    field: "nested".into(),
                    path: vec!["x".into()],
                    operator: FilterOperator::Between,
                    value: "5".into(),
                    second_value: "19".into(),
                },
                FilterCondition {
                    field: "id".into(),
                    operator: FilterOperator::NotEq,
                    value: "13".into(),
                    ..Default::default()
                },
                FilterCondition {
                    field: "title".into(),
                    operator: FilterOperator::Contains,
                    value: "O'Brien 上海".into(),
                    ..Default::default()
                },
            ],
        }
        .to_filter(&engine.info.columns)?;
        let rows = |result: &QueryResult| {
            (0..result.row_count())
                .map(|row| result.source_row_number(row).unwrap())
                .collect::<Vec<_>>()
        };

        let ram = engine.query(&QueryRequest {
            filter: filter.clone(),
            limit: 24,
            ..Default::default()
        })?;
        let expected = (5..20).filter(|&id| id != 13).collect::<Vec<_>>();
        assert_eq!(ids(&ram), expected);
        assert_eq!(ram.loaded_rows, 24);

        // The condition can use a field outside the output projection. A
        // stale range is independent of the explicitly selected source scope.
        let mut request = QueryRequest {
            selected_columns: vec!["id".into(), "title".into()],
            filter: filter.clone(),
            whole_source: true,
            offset: 22,
            limit: 2,
            ..Default::default()
        };
        let whole = engine.query(&request)?;
        assert!(!whole.timings.window_cache_hit);
        assert_eq!(whole.total_rows, 24);
        assert_eq!(whole.loaded_rows, expected.len());
        assert_eq!(whole.filtered_rows, Some(expected.len()));
        assert_eq!(ids(&whole), expected);
        assert_eq!(rows(&whole), rows(&ram));
        for row in 0..whole.row_count() {
            assert_eq!(whole.cell(row, 1).text, ram.cell(row, 1).text);
        }

        request.offset = 3;
        request.limit = 1;
        let repeated = engine.query(&request)?;
        assert!(repeated.timings.window_cache_hit);
        assert_eq!(rows(&repeated), rows(&whole));
        assert!(Arc::ptr_eq(
            repeated.data.raw_batches()[0].column(0),
            whole.data.raw_batches()[0].column(0),
        ));

        request.selected_columns.push("bytes".into());
        let expanded = engine.query(&request)?;
        assert_eq!(ids(&expanded), expected);
        assert_eq!(rows(&expanded), rows(&whole));
        assert_eq!(expanded.binary(0, 2), Some(&[0, 1, 255][..]));

        let summary_request = AggregateRequest {
            metrics: vec![
                AggregateMetric {
                    field: None,
                    op: AggregateOp::CountRows,
                },
                AggregateMetric {
                    field: Some("id".into()),
                    op: AggregateOp::Sum,
                },
            ],
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let summary = engine.aggregate_snapshot_cancellable(
            &expanded.snapshot_id,
            &summary_request,
            &cancel,
        )?;
        assert_eq!(summary.input_rows, 14);
        assert_eq!(summary.data.cell(0, 0).text, "14");
        assert_eq!(summary.data.cell(0, 1).text, "167");

        request.set_sort(
            vec![SortKey {
                field: "id".into(),
                descending: true,
                nulls_first: false,
            }],
            Some(3),
        );
        let top = engine.query(&request)?;
        assert!(top.timings.window_cache_hit);
        assert_eq!(top.loaded_rows, 14);
        assert_eq!(ids(&top), [19, 18, 17]);
        assert_eq!(rows(&top), [20, 19, 18]);

        let range = engine.query(&QueryRequest {
            filter,
            offset: 12,
            limit: 4,
            ..Default::default()
        })?;
        assert!(!range.timings.window_cache_hit);
        assert_eq!(range.loaded_rows, 4);
        assert_eq!(ids(&range), [12, 14, 15]);
        assert_eq!(rows(&range), [13, 15, 16]);

        // The retained Top N snapshot still aggregates its displayed values
        // after the current view switches back to a loaded range.
        let summary =
            engine.aggregate_snapshot_cancellable(&top.snapshot_id, &summary_request, &cancel)?;
        assert_eq!(summary.input_rows, 3);
        assert_eq!(summary.data.cell(0, 0).text, "3");
        assert_eq!(summary.data.cell(0, 1).text, "54");
        Ok(())
    }

    #[test]
    fn top_n_uses_all_sort_keys_after_filter_and_keeps_the_same_export_and_summary() -> Result<()> {
        use crate::aggregation::{AggregateMetric, AggregateOp, AggregateRequest};

        let directory = tempfile::tempdir()?;
        let source = directory.path().join("top-n.parquet");
        let batch = RecordBatch::try_from_iter([
            (
                "id",
                Arc::new(Int64Array::from_iter_values(0..9)) as ArrayRef,
            ),
            (
                "category",
                Arc::new(StringArray::from(vec![
                    Some("b"),
                    Some("a"),
                    Some("A"),
                    Some("A"),
                    Some("B"),
                    Some("a"),
                    None,
                    Some("b"),
                    Some("A"),
                ])) as ArrayRef,
            ),
            (
                "score",
                Arc::new(Int32Array::from(vec![
                    Some(7),
                    Some(9),
                    Some(10),
                    Some(9),
                    Some(7),
                    None,
                    Some(12),
                    Some(8),
                    Some(10),
                ])) as ArrayRef,
            ),
        ])?;
        write_fixture(&source, &batch, 2)?;
        let mut engine = Engine::open(&[source])?;
        let mut request = QueryRequest {
            offset: 1,
            limit: 8,
            filter: "id <> 2".into(),
            sort_keys: vec![
                SortKey {
                    field: "category".into(),
                    descending: false,
                    nulls_first: false,
                },
                SortKey {
                    field: "score".into(),
                    descending: true,
                    nulls_first: true,
                },
            ],
            ..Default::default()
        };
        let all = engine.query(&request)?;
        assert_eq!(ids(&all), [5, 8, 1, 3, 7, 4, 6]);
        assert_eq!(all.filtered_rows, Some(7));
        request.top_n = Some(4);
        let capped = engine.query(&request)?;
        assert_eq!(ids(&capped), [5, 8, 1, 3]);
        assert_eq!(capped.loaded_rows, 8);
        assert_eq!(capped.total_rows, 9);
        assert_eq!(capped.filtered_rows, None);
        assert_eq!(
            (0..capped.row_count())
                .map(|row| capped.source_row_number(row))
                .collect::<Vec<_>>(),
            [Some(6), Some(9), Some(2), Some(4)]
        );
        assert!(Arc::ptr_eq(
            all.data.raw_batches()[0].column(0),
            capped.data.raw_batches()[0].column(0)
        ));

        let summary = engine.aggregate_snapshot_cancellable(
            &capped.snapshot_id,
            &AggregateRequest {
                group_by: vec!["category".into()],
                group_buckets: Vec::new(),
                metrics: vec![AggregateMetric {
                    field: None,
                    op: AggregateOp::CountRows,
                }],
            },
            &AtomicBool::new(false),
        )?;
        assert_eq!(summary.input_rows, 4);
        assert_eq!(summary.data.row_count(), 2);
        assert_eq!(summary.data.cell(0, 1).text, "2");
        assert_eq!(summary.data.cell(1, 1).text, "2");

        request.set_sort(Vec::new(), None);
        let cleared = engine.query(&request)?;
        assert_eq!(ids(&cleared), [1, 3, 4, 5, 6, 7, 8]);
        let json = directory.path().join("top-n.json");
        engine.export_result(&capped.snapshot_id, &json, ExportFormat::Json)?;
        let values: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(json)?)?;
        assert_eq!(
            values
                .iter()
                .map(|row| row["id"].as_i64())
                .collect::<Vec<_>>(),
            [Some(5), Some(8), Some(1), Some(3)]
        );
        Ok(())
    }

    #[test]
    fn text_sort_keeps_case_insensitive_stable_order_and_nulls_last() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("case-order.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("title", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from_iter_values(0..5)),
                Arc::new(StringArray::from(vec![
                    Some("b"),
                    Some("A"),
                    Some("a"),
                    Some("B"),
                    None,
                ])),
            ],
        )?;
        write_fixture(&source, &batch, 2)?;
        let mut engine = Engine::open(&[source])?;
        for (descending, expected) in [(false, vec![1, 2, 0, 3, 4]), (true, vec![0, 3, 1, 2, 4])] {
            let result = engine.query(&QueryRequest {
                sort: Some(("title".into(), descending)),
                ..Default::default()
            })?;
            assert_eq!(ids(&result), expected);
            assert!(result.is_null(result.row_count() - 1, 1));
            assert_eq!(result.source_row_number(0), Some(expected[0] as usize + 1));
        }
        Ok(())
    }

    #[test]
    fn cached_filters_and_clear_use_owned_ram_even_after_source_disappears() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("cached.parquet");
        fixture(&source, 0, 12)?;
        let mut engine = Engine::open(std::slice::from_ref(&source))?;
        let original = engine.query(&QueryRequest {
            limit: 12,
            ..Default::default()
        })?;
        assert!(
            engine.runtime.is_none(),
            "Plain imports should not start a query runtime"
        );
        fs::remove_file(source)?;
        let filtered = engine.query(&QueryRequest {
            limit: 12,
            filter: "id IN (1,5,9)".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&filtered), [1, 5, 9]);
        assert!(filtered.timings.window_cache_hit);
        assert_eq!(filtered.timings.window_load_ms, 0.);
        assert!(Arc::ptr_eq(
            original.data.raw_batches()[0].column(2),
            filtered.data.raw_batches()[0].column(2)
        ));
        let cleared = engine.query(&QueryRequest {
            limit: 12,
            ..Default::default()
        })?;
        assert_eq!(ids(&cleared), (0..12).collect::<Vec<_>>());
        assert!(cleared.timings.window_cache_hit);
        assert!(
            engine.window.as_ref().unwrap().context.is_none(),
            "Clearing processing releases the query provider"
        );
        assert!(
            engine
                .query(&QueryRequest {
                    offset: 1,
                    limit: 3,
                    ..Default::default()
                })
                .is_err()
        );
        assert_eq!(engine.window.as_ref().unwrap().key.limit, 12);
        assert_eq!(
            engine
                .query(&QueryRequest {
                    limit: 12,
                    filter: "id=5".into(),
                    ..Default::default()
                })?
                .source_row_number(0),
            Some(6)
        );
        drop(engine);
        assert_eq!(filtered.cell(2, 0).text, "9");
        Ok(())
    }

    #[test]
    fn field_cache_projects_reorders_and_adds_only_missing_columns_across_chunks() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("field-cache.parquet");
        fixture(&source, 0, 12)?;
        let mut engine = Engine::open(std::slice::from_ref(&source))?;
        engine.source.set_test_batch_rows(3);
        let original = engine.query(&QueryRequest {
            selected_columns: vec!["id".into()],
            offset: 1,
            limit: 9,
            filter: "id>=3".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&original), [3, 4, 5, 6, 7, 8, 9]);
        let old_base = engine.window.as_ref().unwrap().base.clone();
        assert_eq!(old_base.raw_batches()[0].num_rows(), 3);
        let old_id = old_base.raw_batches()[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        // Real Parquet reads with distinct batch boundaries exercise shared
        // slicing rather than a zip of equal-sized decoded RecordBatches.
        engine.source.set_test_batch_rows(2);
        let expanded = engine.query(&QueryRequest {
            selected_columns: vec!["title".into(), "id".into()],
            offset: 1,
            limit: 9,
            sort: Some(("id".into(), true)),
            ..Default::default()
        })?;
        assert!(!expanded.timings.window_cache_hit);
        assert_eq!(ids(&expanded), [9, 8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(expanded.source_row_number(0), Some(10));
        let base = &engine.window.as_ref().unwrap().base;
        assert_eq!(
            base.schema().fields().len(),
            2,
            "Unrequested fields stay unread"
        );
        assert_eq!(base.raw_batches()[0].num_rows(), 2);
        assert_eq!(base.raw_batches()[1].num_rows(), 1);
        let retained_id = base.raw_batches()[0]
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(old_id.values().as_ptr(), retained_id.values().as_ptr());
        assert_eq!(expanded.cell(0, 0).text, "row 9 O'Brien 上海");

        fs::remove_file(&source)?;
        let reordered = engine.query(&QueryRequest {
            selected_columns: vec!["id".into(), "title".into()],
            offset: 1,
            limit: 9,
            filter: "id IN (2,7)".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&reordered), [2, 7]);
        assert_eq!(reordered.source_row_number(1), Some(8));
        assert!(reordered.timings.window_cache_hit);
        assert_eq!(reordered.timings.window_load_ms, 0.);
        let hidden = engine.query(&QueryRequest {
            selected_columns: vec!["id".into()],
            offset: 1,
            limit: 9,
            ..Default::default()
        })?;
        assert_eq!(ids(&hidden), (1..10).collect::<Vec<_>>());
        assert!(hidden.timings.window_cache_hit);
        assert_eq!(
            engine.window.as_ref().unwrap().base.schema().fields().len(),
            1
        );
        assert!(
            engine
                .query(&QueryRequest {
                    selected_columns: vec!["id".into()],
                    offset: 1,
                    limit: 9,
                    filter: "title IS NOT NULL".into(),
                    ..Default::default()
                })
                .is_err(),
            "An old snapshot's hidden field must not enter the current provider"
        );
        assert!(
            engine
                .query(&QueryRequest {
                    selected_columns: vec!["id".into(), "title".into()],
                    offset: 1,
                    limit: 9,
                    ..Default::default()
                })
                .is_err()
        );
        assert_eq!(engine.window.as_ref().unwrap().key.columns, ["id"]);
        assert_eq!(
            engine.window.as_ref().unwrap().base.schema().fields().len(),
            1
        );
        assert_eq!(
            engine.current_snapshot.as_deref(),
            Some(hidden.snapshot_id.as_str())
        );

        // Snapshot schemas stay independent of changes to the current cache.
        let exported = directory.path().join("original-id.parquet");
        engine.export_result(&original.snapshot_id, &exported, ExportFormat::Parquet)?;
        let mut reopened = Engine::open(&[exported])?;
        let roundtrip = reopened.query(&QueryRequest::default())?;
        assert_eq!(roundtrip.columns.len(), 1);
        assert_eq!(ids(&roundtrip), ids(&original));
        let expanded_json = directory.path().join("expanded.json");
        engine.export_result(&expanded.snapshot_id, &expanded_json, ExportFormat::Json)?;
        let rows: serde_json::Value = serde_json::from_reader(File::open(expanded_json)?)?;
        assert_eq!(rows[0]["id"], 9);
        assert_eq!(rows[0].as_object().unwrap().len(), 2);

        // The original snapshot still owns this range and selection.
        engine.discard_snapshot(&hidden.snapshot_id)?;
        engine.discard_snapshot(&reordered.snapshot_id)?;
        engine.discard_snapshot(&expanded.snapshot_id)?;
        assert!(engine.window.is_some());
        engine.discard_snapshot(&original.snapshot_id)?;
        assert!(engine.window.is_none());
        drop(engine);
        assert_eq!(expanded.cell(0, 0).text, "row 9 O'Brien 上海");
        Ok(())
    }

    #[test]
    fn sorting_creates_spill_workspace_only_on_demand_and_releases_workers() -> Result<()> {
        use crate::aggregation::{AggregateMetric, AggregateOp, AggregateRequest};

        let directory = tempfile::tempdir()?;
        let source = directory.path().join("query-reclaim.parquet");
        fixture(&source, 0, 12)?;
        let mut engine = Engine::open(&[source])?;
        let mut request = QueryRequest {
            selected_columns: vec!["id".into()],
            limit: 12,
            sort: Some(("id".into(), true)),
            ..Default::default()
        };
        let sorted = engine.query(&request)?;
        let (manager, spill_path) = {
            let sort_context = &engine.window.as_ref().unwrap().context.as_ref().unwrap().0;
            let environment = sort_context.runtime_env();
            let disk_manager = &environment.disk_manager;
            assert!(disk_manager.tmp_files_enabled());
            assert!(disk_manager.temp_dir_paths().is_empty());
            let _spill = disk_manager.create_tmp_file("test sort spill cleanup")?;
            let spill_path = disk_manager.temp_dir_paths().pop().unwrap();
            assert!(spill_path.is_dir());
            (Arc::downgrade(disk_manager), spill_path)
        };
        assert!(engine.runtime.is_some());
        assert_eq!(ids(&sorted), (0..12).rev().collect::<Vec<_>>());

        // Removing the sort leaves the same resident values but no provider.
        request.sort = None;
        let unsorted = engine.query(&request)?;
        assert_eq!(ids(&unsorted), (0..12).collect::<Vec<_>>());
        assert!(engine.runtime.is_none());
        assert!(manager.upgrade().is_none());
        assert!(!spill_path.exists());

        // Materialized summaries remain valid after their workers have shut down.
        let summary = engine.aggregate_snapshot_cancellable(
            &unsorted.snapshot_id,
            &AggregateRequest {
                metrics: vec![AggregateMetric {
                    field: Some("id".into()),
                    op: AggregateOp::Sum,
                }],
                ..Default::default()
            },
            &AtomicBool::new(false),
        )?;
        assert_eq!(summary.data.cell(0, 0).text, "66");
        assert!(engine.runtime.is_none());

        request.sort = Some(("id".into(), true));
        let restarted = engine.query(&request)?;
        assert_eq!(ids(&restarted), ids(&sorted));
        Ok(())
    }

    #[test]
    fn deselected_fields_release_buffers_after_the_old_snapshot_is_discarded() -> Result<()> {
        use arrow::array::BinaryBuilder;

        const ROWS: usize = 64;
        const VALUE_BYTES: usize = 512 * 1024;
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("reclaim.parquet");
        let mut payload = BinaryBuilder::with_capacity(ROWS, ROWS * VALUE_BYTES);
        let mut value = vec![0xA5; VALUE_BYTES];
        for row in 0..ROWS {
            value[..8].copy_from_slice(&(row as u64).to_le_bytes());
            payload.append_value(&value);
        }
        let batch = RecordBatch::try_from_iter([
            (
                "id",
                Arc::new(Int64Array::from_iter_values(0..ROWS as i64)) as ArrayRef,
            ),
            ("payload", Arc::new(payload.finish()) as ArrayRef),
        ])?;
        write_fixture(&source, &batch, 16)?;
        drop(batch);

        let mut engine = Engine::open(std::slice::from_ref(&source))?;
        let mut request = QueryRequest {
            selected_columns: vec!["id".into(), "payload".into()],
            limit: ROWS,
            filter: "id >= 2".into(),
            sort: Some(("id".into(), true)),
            top_n: Some(17),
            ..Default::default()
        };
        let wide = engine.query(&request)?;
        let expected = (ROWS as i64 - 17..ROWS as i64).rev().collect::<Vec<_>>();
        assert_eq!(ids(&wide), expected);
        let mut probes = HashMap::new();
        for batch in wide.data.raw_batches() {
            let array = batch.column(1).to_data();
            for buffer in array.buffers() {
                if buffer.capacity() > 0 {
                    probes
                        .entry((buffer.data_ptr().as_ptr() as usize, buffer.capacity()))
                        .or_insert_with(|| buffer.clone());
                }
            }
        }
        let payload_bytes: usize = probes.keys().map(|(_, capacity)| capacity).sum();
        assert!(payload_bytes >= ROWS * VALUE_BYTES);

        // The selected range, filter, sort and Top N can be served from RAM
        // even while the Parquet file is unavailable at its original path.
        let parked = source.with_extension("parked");
        fs::rename(&source, &parked)?;
        request.selected_columns.pop();
        let narrow = engine.query(&request)?;
        assert!(narrow.timings.window_cache_hit);
        assert_eq!(narrow.timings.window_load_ms, 0.);
        assert_eq!(narrow.timings.index_query_ms, 0.);
        assert!(engine.window.as_ref().unwrap().context.is_none());
        assert_eq!(ids(&narrow), expected);
        assert_eq!(narrow.source_row_number(0), Some(ROWS));
        let old_snapshot_id = wide.snapshot_id.clone();
        drop(wide);

        let last_prefix = ((ROWS - 1) as u64).to_le_bytes();
        let old_snapshot = engine.snapshots.get(&old_snapshot_id).unwrap();
        assert_eq!(&old_snapshot.data.binary(0, 1).unwrap()[..8], &last_prefix);
        assert!(probes.values().any(|buffer| buffer.strong_count() > 1));
        engine.discard_snapshot(&old_snapshot_id)?;

        let released_bytes: usize = probes
            .iter()
            .filter(|(_, buffer)| buffer.strong_count() == 1)
            .map(|((_, capacity), _)| capacity)
            .sum();
        assert_eq!(
            released_bytes, payload_bytes,
            "Superseded payload buffers still have an owner beyond the probe"
        );
        drop(probes);
        eprintln!("Reclaimed {released_bytes} unique Arrow payload buffer bytes");

        fs::rename(&parked, &source)?;
        request.selected_columns.push("payload".into());
        let reselected = engine.query(&request)?;
        assert!(!reselected.timings.window_cache_hit);
        assert_eq!(ids(&reselected), expected);
        assert_eq!(reselected.source_row_number(0), Some(ROWS));
        assert_eq!(&reselected.binary(0, 1).unwrap()[..8], &last_prefix);
        for (before, after) in narrow
            .data
            .raw_batches()
            .iter()
            .zip(reselected.data.raw_batches())
        {
            assert!(Arc::ptr_eq(before.column(0), after.column(0)));
        }
        // A rejected wider result must not retain its cache. The accepted
        // narrow result can rebuild the cache from its immutable snapshot.
        let rejected_payload = Arc::downgrade(reselected.data.raw_batches()[0].column(1));
        engine.discard_snapshot(&reselected.snapshot_id)?;
        drop(reselected);
        assert!(engine.window.is_none());
        assert!(rejected_payload.upgrade().is_none());
        engine.restore_displayed_window(&narrow.snapshot_id);
        fs::remove_file(source)?;
        request.selected_columns.pop();
        request.top_n = None;
        let restored = engine.query(&request)?;
        assert_eq!(restored.row_count(), ROWS - 2);
        assert_eq!(restored.source_row_number(0), Some(ROWS));
        assert!(restored.timings.window_cache_hit);
        Ok(())
    }

    #[test]
    fn changed_source_during_missing_field_load_keeps_cached_window_and_snapshots() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("changed-fields.parquet");
        fixture(&source, 0, 12)?;
        let mut engine = Engine::open(std::slice::from_ref(&source))?;
        let original = engine.query(&QueryRequest {
            selected_columns: vec!["id".into()],
            limit: 12,
            filter: "id>=10".into(),
            ..Default::default()
        })?;
        let cached_field = Arc::clone(original.data.raw_batches()[0].column(0));
        fs::rename(&source, directory.path().join("old-source.parquet"))?;
        let schema = Arc::new(Schema::new(vec![Field::new(
            "other",
            DataType::Int64,
            false,
        )]));
        write_fixture(
            &source,
            &RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![99]))])?,
            1,
        )?;
        assert!(
            engine
                .query(&QueryRequest {
                    selected_columns: vec!["id".into(), "title".into()],
                    limit: 12,
                    ..Default::default()
                })
                .is_err()
        );
        assert_eq!(
            engine.current_snapshot.as_deref(),
            Some(original.snapshot_id.as_str())
        );
        let cached = engine.window.as_ref().unwrap();
        assert_eq!(cached.key.columns, ["id"]);
        assert_eq!(cached.base.schema().fields().len(), 1);
        assert!(Arc::ptr_eq(
            cached.base.raw_batches()[0].column(0),
            &cached_field
        ));
        let still_owned = engine.query(&QueryRequest {
            selected_columns: vec!["id".into()],
            limit: 12,
            filter: "id=11".into(),
            ..Default::default()
        })?;
        assert!(still_owned.timings.window_cache_hit);
        assert_eq!(ids(&still_owned), [11]);
        assert_eq!(still_owned.source_row_number(0), Some(12));
        Ok(())
    }

    #[test]
    fn interrupted_missing_field_query_restores_window_and_active_selection() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("cancel-added-field.parquet");
        let rows = 100_000;
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("added", DataType::Int64, false),
            ])),
            vec![
                Arc::new(Int64Array::from_iter_values(0..rows)),
                Arc::new(Int64Array::from_iter_values((0..rows).rev())),
            ],
        )?;
        write_fixture(&source, &batch, 16384)?;
        let mut engine = Engine::open(std::slice::from_ref(&source))?;
        let original = engine.query(&QueryRequest {
            selected_columns: vec!["id".into()],
            limit: rows as usize,
            ..Default::default()
        })?;
        let progress = QueryProgress::default();
        let observer = progress.clone();
        let handle = engine.interrupt_handle();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        let canceller = std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                if observer.snapshot().phase == QueryPhase::Sorting {
                    handle.interrupt();
                    return true;
                }
                std::thread::yield_now();
            }
            false
        });
        let result = engine.query_cancellable_with_progress(
            &QueryRequest {
                selected_columns: vec!["id".into(), "added".into()],
                limit: rows as usize,
                sort: Some(("added".into(), true)),
                ..Default::default()
            },
            &AtomicBool::new(false),
            &progress,
        );
        finished.store(true, Ordering::Release);
        assert!(
            canceller.join().unwrap(),
            "Missing fields must be decoded before interruption"
        );
        assert!(result.is_err());
        let cache = engine.window.as_ref().unwrap();
        assert_eq!(cache.key.columns, ["id"]);
        assert_eq!(cache.base.schema().fields().len(), 1);
        assert!(Arc::ptr_eq(
            cache.base.raw_batches()[0].column(0),
            original.data.raw_batches()[0].column(0)
        ));
        assert_eq!(engine.snapshots.len(), 1);
        assert_eq!(
            engine.current_snapshot.as_deref(),
            Some(original.snapshot_id.as_str())
        );
        fs::remove_file(source)?;
        let usable = engine.query(&QueryRequest {
            selected_columns: vec!["id".into()],
            limit: rows as usize,
            filter: "id>=99998".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&usable), [99998, 99999]);
        assert_eq!(usable.source_row_number(1), Some(100000));
        assert!(usable.timings.window_cache_hit);
        Ok(())
    }

    #[test]
    fn old_snapshots_export_after_new_windows_failed_queries_and_cancel() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("snapshots.parquet");
        fixture(&source, 0, 30)?;
        let mut engine = Engine::open(std::slice::from_ref(&source))?;
        let first = engine.query(&QueryRequest {
            offset: 2,
            limit: 5,
            sort: Some(("id".into(), true)),
            ..Default::default()
        })?;
        let second = engine.query(&QueryRequest {
            offset: 10,
            limit: 7,
            filter: "id>12".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&second), [13, 14, 15, 16]);
        assert!(
            engine
                .query(&QueryRequest {
                    filter: "unavailable_field=1".into(),
                    ..Default::default()
                })
                .is_err()
        );
        assert!(
            engine
                .query_cancellable(&QueryRequest::default(), &AtomicBool::new(true))
                .is_err()
        );
        assert_eq!(
            engine.current_snapshot.as_deref(),
            Some(second.snapshot_id.as_str())
        );
        fs::remove_file(source)?;
        let destination = directory.path().join("exact-old.parquet");
        engine.export_result_columns_cancellable(
            &first.snapshot_id,
            &["nested".into(), "id".into(), "amount".into()],
            &destination,
            ExportFormat::Parquet,
            &AtomicBool::new(false),
        )?;
        let mut reopened = Engine::open(&[destination])?;
        let exported = reopened.query(&QueryRequest::default())?;
        assert_eq!(
            exported
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["nested", "id", "amount"]
        );
        assert_eq!(ids(&exported), [6, 5, 4, 3, 2]);
        assert_eq!(exported.cell(0, 2).text, first.cell(0, 4).text);
        engine.discard_snapshot(&second.snapshot_id)?;
        assert!(
            engine.window.is_none(),
            "An unpublished/discarded window must not pin another dataset"
        );
        assert_eq!(first.cell(0, 0).text, "6");
        engine.export_result(
            &first.snapshot_id,
            &directory.path().join("old-again.json"),
            ExportFormat::Json,
        )?;
        Ok(())
    }

    fn typed_fixture(path: &Path) -> Result<()> {
        let ids: ArrayRef = Arc::new(Int64Array::from_iter_values(0..6));
        let decimal = 9007199254740993123456789012345678i128;
        let amounts: ArrayRef = Arc::new(
            Decimal128Array::from(vec![
                Some(decimal),
                Some(decimal + 1),
                None,
                Some(decimal + 2),
                Some(decimal - 1),
                Some(decimal + 10),
            ])
            .with_precision_and_scale(38, 18)?,
        );
        let timestamp =
            chrono::DateTime::parse_from_rfc3339("2024-02-28T12:34:56.123456789+00:00")?
                .timestamp_nanos_opt()
                .unwrap();
        let stamps: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![
            Some(timestamp),
            Some(timestamp + 86_400_000_000_000),
            None,
            Some(timestamp + 1000),
            Some(timestamp + 1),
            Some(timestamp + 172_800_000_000_000),
        ]));
        let profile: ArrayRef = Arc::new(StructArray::try_new(
            vec![Arc::new(Field::new("age", DataType::Int32, true))].into(),
            vec![Arc::new(Int32Array::from(vec![
                Some(10),
                Some(20),
                Some(30),
                Some(40),
                None,
                Some(60),
            ]))],
            Some(NullBuffer::from(vec![true, true, false, true, true, true])),
        )?);
        let items: ArrayRef = Arc::new(ListArray::from_iter_primitive::<Int64Type, _, _>(vec![
            Some(vec![Some(1), None]),
            Some(vec![]),
            None,
            Some(vec![Some(2)]),
            Some(vec![None]),
            Some(vec![Some(9007199254740993)]),
        ]));
        let decimal_values = Arc::new(
            Decimal128Array::from(vec![
                Some(decimal + 1),
                Some(decimal),
                None,
                Some(decimal + 10),
            ])
            .with_precision_and_scale(38, 18)?,
        );
        let decimal_items: ArrayRef = Arc::new(ListArray::new(
            Arc::new(Field::new("item", DataType::Decimal128(38, 18), true)),
            OffsetBuffer::new(vec![0, 1, 1, 1, 2, 3, 4].into()),
            decimal_values,
            Some(NullBuffer::from(vec![true, true, false, true, true, true])),
        ));
        let active: ArrayRef = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(false),
            None,
            Some(true),
            Some(false),
            Some(true),
        ]));
        let titles: ArrayRef = Arc::new(StringArray::from(vec![
            Some("123"),
            Some("O'Brien"),
            Some("上海"),
            Some(""),
            None,
            Some("42"),
        ]));
        let arrays = vec![
            ids,
            amounts,
            stamps,
            profile,
            items,
            decimal_items,
            active,
            titles,
        ];
        let names = [
            "id",
            "amount",
            "stamp",
            "profile",
            "items",
            "decimal_items",
            "active",
            "numeric_text",
        ];
        let schema = Arc::new(Schema::new(
            names
                .into_iter()
                .zip(&arrays)
                .map(|(name, array)| Field::new(name, array.data_type().clone(), true))
                .collect::<Vec<_>>(),
        ));
        write_fixture(path, &RecordBatch::try_new(schema, arrays)?, 2)
    }

    #[test]
    fn typed_ui_conditions_preserve_decimal_nanoseconds_struct_and_list_semantics() -> Result<()> {
        use crate::filtering::{FilterCondition, FilterGroup, FilterOperator as Op, MatchMode};
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("typed.parquet");
        typed_fixture(&source)?;
        let mut engine = Engine::open(&[source])?;
        for (field, path, operator, value, expected) in [
            (
                "amount",
                vec![],
                Op::Eq,
                "9007199254740993.123456789012345679",
                vec![1],
            ),
            (
                "stamp",
                vec![],
                Op::Eq,
                "2024-02-28 12:34:56.123456789",
                vec![0],
            ),
            ("profile", vec!["age"], Op::Gt, "15", vec![1, 3, 5]),
            ("profile", vec!["age"], Op::IsNull, "", vec![2, 4]),
            ("profile", vec![], Op::IsNull, "", vec![2]),
            ("items", vec![], Op::Contains, "9007199254740993", vec![5]),
            ("items", vec![], Op::Contains, "2", vec![3]),
            ("items", vec![], Op::NotContains, "2", vec![0, 1, 4, 5]),
            ("items", vec![], Op::IsEmpty, "", vec![1]),
            ("items", vec![], Op::IsNotEmpty, "", vec![0, 3, 4, 5]),
            ("items", vec![], Op::IsNull, "", vec![2]),
            (
                "decimal_items",
                vec![],
                Op::Contains,
                "9007199254740993.123456789012345679",
                vec![0],
            ),
            ("active", vec![], Op::Eq, "true", vec![0, 3, 5]),
            ("numeric_text", vec![], Op::Contains, "2", vec![0, 5]),
        ] {
            let group = FilterGroup {
                mode: MatchMode::All,
                conditions: vec![FilterCondition {
                    field: field.into(),
                    path: path.into_iter().map(str::to_owned).collect(),
                    operator,
                    value: value.into(),
                    second_value: String::new(),
                }],
            };
            let filter = group.to_filter(&engine.info.columns)?;
            let result = engine
                .query(&QueryRequest {
                    filter: filter.clone(),
                    ..Default::default()
                })
                .with_context(|| format!("{field} {operator:?}: {filter}"))?;
            assert_eq!(ids(&result), expected, "{field} {operator:?}: {filter}");
            for row in 0..result.row_count() {
                assert_eq!(
                    result.source_row_number(row),
                    Some(ids(&result)[row] as usize + 1)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn empty_views_and_empty_sources_preserve_typed_schema_for_export() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("empty-view.parquet");
        fixture(&source, 0, 6)?;
        let mut engine = Engine::open(&[source])?;
        let empty = engine.query(&QueryRequest {
            filter: "id<0".into(),
            ..Default::default()
        })?;
        assert_eq!(empty.row_count(), 0);
        assert_eq!(empty.columns.len(), 6);
        assert_eq!(empty.source_row_number(0), None);
        let output = directory.path().join("zero.parquet");
        engine.export_result(&empty.snapshot_id, &output, ExportFormat::Parquet)?;
        let mut reopened = Engine::open(&[output])?;
        let again = reopened.query(&QueryRequest {
            filter: "id IS NULL".into(),
            ..Default::default()
        })?;
        assert_eq!(again.row_count(), 0);
        assert_eq!(again.columns, empty.columns);
        Ok(())
    }

    #[test]
    fn parallel_engines_keep_independent_datasets_and_exportable_snapshots() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for base in [0, 1000] {
            let source = directory.path().join(format!("source-{base}.parquet"));
            fixture(&source, base, 20)?;
            let mut engine = Engine::open(&[source])?;
            let destination = directory.path().join(format!("export-{base}.parquet"));
            let start = barrier.clone();
            threads.push(std::thread::spawn(move || -> Result<()> {
                start.wait();
                let result = engine.query(&QueryRequest {
                    selected_columns: vec!["id".into()],
                    limit: 10,
                    filter: format!("id>={}", base + 5),
                    ..Default::default()
                })?;
                assert_eq!(ids(&result), (base + 5..base + 10).collect::<Vec<_>>());
                engine.export_result(&result.snapshot_id, &destination, ExportFormat::Parquet)?;
                let mut reopened = Engine::open(&[destination])?;
                assert_eq!(
                    ids(&reopened.query(&QueryRequest::default())?),
                    ids(&result)
                );
                Ok(())
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap()?;
        }
        Ok(())
    }

    #[test]
    fn interrupt_after_loading_rolls_back_window_and_leaves_next_query_usable() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("cancel.parquet");
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(Int64Array::from_iter_values(0..1_000_000))],
        )?;
        write_fixture(&source, &batch, 16384)?;
        drop(batch);
        let mut engine = Engine::open(std::slice::from_ref(&source))?;
        let old = engine.query(&QueryRequest {
            limit: 100,
            ..Default::default()
        })?;
        let progress = QueryProgress::default();
        let observer = progress.clone();
        let handle = engine.interrupt_handle();
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        let canceller = std::thread::spawn(move || {
            while !finished.load(Ordering::Acquire) {
                if observer.snapshot().phase == QueryPhase::Sorting {
                    handle.interrupt();
                    return true;
                }
                std::thread::yield_now();
            }
            false
        });
        let result = engine.query_cancellable_with_progress(
            &QueryRequest {
                limit: 1_000_000,
                sort: Some(("id".into(), true)),
                ..Default::default()
            },
            &AtomicBool::new(false),
            &progress,
        );
        done.store(true, Ordering::Release);
        assert!(
            canceller.join().unwrap(),
            "Expected real source loading before cancellation"
        );
        assert!(
            result.is_err(),
            "Interrupted query must not publish a snapshot"
        );
        assert_eq!(
            engine.current_snapshot.as_deref(),
            Some(old.snapshot_id.as_str())
        );
        assert_eq!(engine.window.as_ref().unwrap().key.limit, 100);
        fs::remove_file(source)?;
        let next = engine.query(&QueryRequest {
            limit: 100,
            filter: "id>=98".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&next), [98, 99]);
        drop(engine);
        assert_eq!(old.cell(99, 0).text, "99");
        Ok(())
    }

    #[test]
    fn expression_protocol_rejects_side_effects_but_preserves_quoted_values() -> Result<()> {
        for expression in [
            "id IN (SELECT id FROM loaded_window)",
            "1; DROP TABLE loaded_window",
            "read_parquet('/tmp/input') IS NOT NULL",
            "id=1 -- comment",
            "id=1 /* comment */",
            "load_extension('x')=1",
            "EXISTS(SELECT 1)",
            "ATTACH '/tmp/database'",
            "unregistered(id)=1",
            "'unterminated",
        ] {
            assert!(validated_filter(expression).is_err(), "{expression}");
        }
        assert_eq!(
            validated_filter(" WHERE \"FROM; field\" = 'O''Brien; -- DROP' ")?,
            "\"FROM; field\" = 'O''Brien; -- DROP'"
        );
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("quoted.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("FROM; \"field\"", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec!["O'Brien; -- DROP", "literal"])),
            ],
        )?;
        write_fixture(&source, &batch, 1)?;
        let mut engine = Engine::open(&[source])?;
        let result = engine.query(&QueryRequest {
            filter: "\"FROM; \"\"field\"\"\" = 'O''Brien; -- DROP'".into(),
            ..Default::default()
        })?;
        assert_eq!(ids(&result), [1]);
        assert_eq!(engine.query(&QueryRequest::default())?.row_count(), 2);
        assert_eq!(worker_count(0), 1);
        assert_eq!(worker_count(64), 64);
        assert_eq!(workspace_bytes(usize::MAX), 4096usize * 1024 * 1024);
        Ok(())
    }
}
