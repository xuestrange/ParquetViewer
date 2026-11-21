//! Key audits and version comparison over projected Arrow data.
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use arrow::{
    array::{Array, ArrayRef, BooleanArray, UInt64Array, make_comparator},
    buffer::{BooleanBuffer, NullBuffer},
    compute::{CastOptions, SortOptions, cast_with_options, filter, kernels::cmp::distinct, or},
    datatypes::{DataType, Field, Schema, SchemaRef},
    record_batch::RecordBatch,
};
use datafusion::{
    catalog::streaming::StreamingTable,
    datasource::MemTable,
    execution::{TaskContext, memory_pool::GreedyMemoryPool, runtime_env::RuntimeEnvBuilder},
    physical_plan::{
        SendableRecordBatchStream, stream::RecordBatchStreamAdapter, streaming::PartitionStream,
    },
    prelude::{SessionConfig, SessionContext},
};
use futures::TryStreamExt;
use rayon::prelude::*;

use crate::{
    columnar::{ArrowTable, RowIndexChunk},
    engine::{
        QueryResult,
        csv_source::{CsvOptions, CsvProbe, CsvSource, detect_path_delimiter},
        is_parquet_file,
    },
    filtering::{FilterFieldKind, field_kind},
    resource_budget::Client,
    source_loader::ParquetSource,
};

const BATCH_ROWS: usize = 8192;
const COMPARE_COLUMNS: usize = 4;

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    Ok(())
}

pub(crate) fn supports_key(data_type: &str) -> bool {
    matches!(
        field_kind(data_type),
        FilterFieldKind::Number
            | FilterFieldKind::Text
            | FilterFieldKind::Boolean
            | FilterFieldKind::Date
            | FilterFieldKind::DateTime
            | FilterFieldKind::Time
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SchemaChange {
    pub field: String,
    pub original: Option<String>,
    pub revised: Option<String>,
}

pub(crate) struct VersionSources {
    old: VersionSource,
    new: VersionSource,
    pub schema_changes: Vec<SchemaChange>,
    pub keys: Vec<String>,
    key_types: HashMap<String, DataType>,
    value_columns: Vec<String>,
}

enum VersionSource {
    Csv(Box<CsvSource>),
    Parquet(ParquetSource),
}

impl VersionSource {
    fn is_csv(&self) -> bool {
        matches!(self, Self::Csv(_))
    }

    fn open(path: &Path, csv_options: Option<CsvOptions>, cancel: &AtomicBool) -> Result<Self> {
        if csv_options.is_none() && is_parquet_file(path) {
            Ok(Self::Parquet(ParquetSource::open(
                &[path.to_owned()],
                cancel,
            )?))
        } else {
            Ok(Self::Csv(Box::new(CsvSource::open(
                path,
                csv_options,
                cancel,
                |_, _| {},
            )?)))
        }
    }

    fn open_revised(path: &Path, original: &Self, cancel: &AtomicBool) -> Result<Self> {
        let Self::Csv(original) = original else {
            return Self::open(path, None, cancel);
        };
        if is_parquet_file(path) {
            return Self::open(path, None, cancel);
        }
        let mut options = original.options.clone();
        options.delimiter = detect_path_delimiter(path, Some(options.delimiter), cancel)?;
        let overrides = std::mem::take(&mut options.column_types);
        if !overrides.is_empty() {
            // Override positions belong to the original header. Match by name so
            // that inserting or moving a column cannot change another field's type.
            let probe = CsvProbe::read(path, Some(options.clone()), cancel)?;
            let names = original.info.columns.iter().map(|column| &column.name);
            let overrides = names
                .enumerate()
                .filter_map(|(index, name)| {
                    overrides.get(&index).map(|kind| (name.as_str(), *kind))
                })
                .collect::<HashMap<_, _>>();
            options.column_types = probe
                .columns
                .iter()
                .enumerate()
                .filter_map(|(index, column)| {
                    overrides
                        .get(column.name.as_str())
                        .map(|kind| (index, *kind))
                })
                .collect();
        }
        Self::open(path, Some(options), cancel)
    }

    fn schema(&self) -> SchemaRef {
        match self {
            Self::Csv(source) => source.schema(),
            Self::Parquet(source) => source.schema(),
        }
    }

    fn load(
        &self,
        fields: &[String],
        resources: &Client,
        cancel: &AtomicBool,
    ) -> Result<ArrowTable> {
        match self {
            Self::Csv(source) => {
                source.load_window(fields, 0, usize::MAX, resources, cancel, |_| {}, |_, _| {})
            }
            Self::Parquet(source) => source.load_window(
                fields,
                0,
                source.info.row_count.max(1),
                resources,
                cancel,
                |_| {},
            ),
        }
    }
}

fn field_description(field: &Field) -> String {
    format!(
        "{} · {}",
        crate::arrow_types::field_type_label(field),
        if field.is_nullable() {
            "NULL"
        } else {
            "NOT NULL"
        },
    )
}

fn is_utf8(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
    )
}

fn common_type(left: &DataType, right: &DataType) -> Option<DataType> {
    use DataType::*;
    if left == right {
        return Some(left.clone());
    }
    if is_utf8(left) && is_utf8(right) {
        return Some(Utf8View);
    }
    if matches!((left, right), (Date32, Date64) | (Date64, Date32)) {
        return Some(Date64);
    }
    if left.is_signed_integer() && right.is_signed_integer() {
        return Some(Int64);
    }
    if left.is_unsigned_integer() && right.is_unsigned_integer() {
        return Some(UInt64);
    }

    // Integers and decimals are comparable only when every possible value of
    // both source types fits in a common exact representation.
    let dimensions = |kind: &DataType| match kind {
        Int8 | UInt8 => Some((3i16, 0i16)),
        Int16 | UInt16 => Some((5, 0)),
        Int32 | UInt32 => Some((10, 0)),
        Int64 => Some((19, 0)),
        UInt64 => Some((20, 0)),
        Decimal32(precision, scale)
        | Decimal64(precision, scale)
        | Decimal128(precision, scale)
        | Decimal256(precision, scale) => Some((i16::from(*precision), i16::from(*scale))),
        _ => None,
    };
    let (p1, s1) = dimensions(left)?;
    let (p2, s2) = dimensions(right)?;
    let scale = s1.max(s2);
    let precision = (p1 - s1).max(p2 - s2) + scale;
    match precision {
        1..=38 => Some(Decimal128(precision as u8, scale as i8)),
        39..=76 => Some(Decimal256(precision as u8, scale as i8)),
        _ => None,
    }
}

fn compare_type(left: &Field, right: &Field, text_source: bool) -> Option<DataType> {
    let same_annotations = [
        "ARROW:extension:name",
        "ARROW:extension:metadata",
        "parquetviewer.logical_type",
    ]
    .iter()
    .all(|key| left.metadata().get(*key) == right.metadata().get(*key));
    if !same_annotations {
        return None;
    }
    if text_source {
        common_type(left.data_type(), right.data_type())
    } else {
        (left.data_type() == right.data_type()).then(|| left.data_type().clone())
    }
}

fn cast_for_comparison(
    array: &ArrayRef,
    target: &DataType,
) -> Result<ArrayRef, arrow::error::ArrowError> {
    if array.data_type() == target {
        return Ok(array.clone());
    }
    cast_with_options(
        array,
        target,
        &CastOptions {
            safe: false,
            ..Default::default()
        },
    )
}

impl VersionSources {
    #[cfg(test)]
    pub fn open(old: PathBuf, new: PathBuf, cancel: &AtomicBool) -> Result<Self> {
        Self::open_with_options(old, new, None, cancel)
    }

    pub fn open_with_options(
        old: PathBuf,
        new: PathBuf,
        original_csv_options: Option<CsvOptions>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let old = VersionSource::open(&old, original_csv_options, cancel)?;
        let new = VersionSource::open_revised(&new, &old, cancel)?;
        let left = old.schema();
        let right = new.schema();
        // Text inference and Parquet schemas can choose different physical
        // encodings for values that have a shared exact representation.
        let text_source = old.is_csv() || new.is_csv();
        let mut keys = Vec::new();
        let mut key_types = HashMap::new();
        let mut value_columns = Vec::new();
        let mut schema_changes = Vec::new();
        let right_by_name = right
            .fields()
            .iter()
            .map(|f| (f.name(), f))
            .collect::<HashMap<_, _>>();
        let left_names = left
            .fields()
            .iter()
            .map(|f| f.name())
            .collect::<HashSet<_>>();
        for field in left.fields() {
            let other = right_by_name.get(field.name());
            if let Some(common) = other.and_then(|other| compare_type(field, other, text_source)) {
                if supports_key(&crate::arrow_types::field_type_label(field)) {
                    keys.push(field.name().clone());
                    key_types.insert(field.name().clone(), common);
                }
                value_columns.push(field.name().clone());
            }
            if other.is_none_or(|other| {
                (field.data_type() != other.data_type()
                    && !(text_source && is_utf8(field.data_type()) && is_utf8(other.data_type())))
                    || compare_type(field, other, text_source).is_none()
                    || field.is_nullable() != other.is_nullable()
            }) {
                let mut original = field_description(field);
                let mut revised = other.map(|field| field_description(field));
                if let Some((other, revised)) = other.zip(revised.as_mut())
                    && original == *revised
                {
                    original = exact_field_description(field);
                    *revised = exact_field_description(other);
                }
                schema_changes.push(SchemaChange {
                    field: field.name().clone(),
                    original: Some(original),
                    revised,
                });
            }
        }
        for field in right
            .fields()
            .iter()
            .filter(|f| !left_names.contains(f.name()))
        {
            schema_changes.push(SchemaChange {
                field: field.name().clone(),
                original: None,
                revised: Some(field_description(field)),
            });
        }
        Ok(Self {
            old,
            new,
            schema_changes,
            keys,
            key_types,
            value_columns,
        })
    }

    pub fn compare(
        &self,
        keys: &[String],
        cancel: &Arc<AtomicBool>,
        on_columns: impl Fn(usize, usize),
    ) -> Result<VersionDiff> {
        check_cancel(cancel)?;
        validate_keys(keys)?;
        let names = key_names(keys.len());
        ensure!(
            keys.iter().all(|key| self.keys.contains(key)),
            "Choose matching scalar key fields."
        );
        let runtime = AuditRuntime::new()?;
        let resources = Client::new();
        let (left, right) = self.load_pair(keys, &resources, cancel)?;
        let key_types = keys
            .iter()
            .map(|key| self.key_types[key].clone())
            .collect::<Vec<_>>();
        runtime.register("audit_left", &left, Some(&key_types), cancel)?;
        runtime.register("audit_right", &right, Some(&key_types), cancel)?;
        // Refuse ambiguous matches before the full join allocates output.
        let null_key = names
            .iter()
            .map(|name| format!("{name} IS NULL"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let key_list = names.join(", ");
        for (table, error) in [
            (
                "audit_left",
                "Original file contains NULL or duplicate keys.",
            ),
            (
                "audit_right",
                "Comparison file contains NULL or duplicate keys.",
            ),
        ] {
            let query = format!(
                "SELECT 1 FROM {table} GROUP BY {key_list} HAVING COUNT(*) > 1 OR {null_key} LIMIT 1"
            );
            ensure!(runtime.query(&query, cancel)?.is_empty(), "{error}");
        }
        let on = join_condition(&names, "=");
        let mut result = VersionDiff::default();
        let mut pairs = Vec::new();
        let mut paired_rows = 0;
        runtime.query_each(
            &format!("SELECT l.row_pos AS old_pos, r.row_pos AS new_pos FROM audit_left l FULL OUTER JOIN audit_right r ON {on}"),
            cancel,
            |batch| {
            let left = positions(batch.column(0))?;
            let right = positions(batch.column(1))?;
            let (matched_left, matched_right) = if let Some(missing) = NullBuffer::union(left.nulls(), right.nulls())
                .filter(|missing| missing.null_count() > 0)
            {
                for (left, right) in left.iter().zip(right.iter()) {
                    match (left, right) {
                        (None, Some(right)) => result.added.push(right),
                        (Some(left), None) => result.removed.push(left),
                        _ => {}
                    }
                }
                if missing.null_count() == batch.num_rows() {
                    return Ok(());
                }
                let both = BooleanArray::from(missing.into_inner());
                (positions(&filter(left, &both)?)?.clone(), positions(&filter(right, &both)?)?.clone())
            } else {
                (left.clone(), right.clone())
            };
            if !matched_left.is_empty() {
                let len = matched_left.len();
                pairs.push(PairedRows {
                    start: paired_rows,
                    changed: BooleanArray::from(BooleanBuffer::new_unset(len)),
                    left: matched_left,
                    right: matched_right,
                });
                paired_rows += len;
            }
            Ok(())
        })?;
        drop(runtime);
        drop((left, right));
        let values = self
            .value_columns
            .iter()
            .filter(|column| !keys.contains(column))
            .cloned()
            .collect::<Vec<_>>();
        on_columns(0, values.len());
        if !pairs.is_empty() {
            for (group, fields) in values.chunks(COMPARE_COLUMNS).enumerate() {
                check_cancel(cancel)?;
                let (left, right) = self.load_pair(fields, &resources, cancel)?;
                let left = left.with_row_index_batches(
                    pairs
                        .iter()
                        .map(|pair| RowIndexChunk::Wide(pair.left.clone()))
                        .collect(),
                )?;
                let right = right.with_row_index_batches(
                    pairs
                        .iter()
                        .map(|pair| RowIndexChunk::Wide(pair.right.clone()))
                        .collect(),
                )?;
                let unchanged = pairs
                    .par_iter_mut()
                    .map(|pair| pair.compare(&left, &right, cancel))
                    .try_reduce(|| 0, |a, b| Ok(a + b))?;
                if unchanged == 0 {
                    on_columns(values.len(), values.len());
                    break;
                }
                on_columns(
                    ((group + 1) * COMPARE_COLUMNS).min(values.len()),
                    values.len(),
                );
            }
        }
        for pair in pairs {
            check_cancel(cancel)?;
            for row in pair.changed.values().set_indices() {
                result
                    .modified
                    .push((pair.left.value(row), pair.right.value(row)));
            }
        }
        result.finish(cancel)?;
        check_cancel(cancel)?;
        Ok(result)
    }

    fn load_pair(
        &self,
        fields: &[String],
        resources: &Client,
        cancel: &AtomicBool,
    ) -> Result<(ArrowTable, ArrowTable)> {
        // Retain only the keys or the current value columns. CSV still parses
        // the record structure to reach the selected fields on each read.
        let left = self.old.load(fields, resources, cancel)?;
        let right = self.new.load(fields, resources, cancel)?;
        Ok((left, right))
    }
}

fn exact_field_description(field: &Field) -> String {
    let mut description = format!("{} · {:?}", field_description(field), field.data_type());
    for key in [
        "ARROW:extension:name",
        "ARROW:extension:metadata",
        "parquetviewer.logical_type",
    ] {
        if let Some(value) = field.metadata().get(key) {
            description.push_str(&format!(" · {value}"));
        }
    }
    description
}

struct PairedRows {
    start: usize,
    left: UInt64Array,
    right: UInt64Array,
    changed: BooleanArray,
}

impl PairedRows {
    fn compare(
        &mut self,
        left: &ArrowTable,
        right: &ArrowTable,
        cancel: &AtomicBool,
    ) -> Result<usize> {
        check_cancel(cancel)?;
        let len = self.left.len();
        let mut unchanged = len - self.changed.true_count();
        if unchanged > 0 {
            let range = self.start..self.start + len;
            let a = left.materialize_range(range.clone())?;
            let b = right.materialize_range(range)?;
            for (a, b) in a.columns().iter().zip(b.columns()) {
                check_cancel(cancel)?;
                self.changed = or(&self.changed, &different(a, b)?)?;
                unchanged = len - self.changed.true_count();
                if unchanged == 0 {
                    break;
                }
            }
        }
        Ok(unchanged)
    }
}

fn different(left: &ArrayRef, right: &ArrayRef) -> Result<BooleanArray> {
    if left.data_type() == &DataType::Null {
        return Ok(BooleanArray::from(BooleanBuffer::new_unset(left.len())));
    }
    if left.data_type() != right.data_type() {
        let common = common_type(left.data_type(), right.data_type())
            .context("Common fields do not have compatible types.")?;
        let left = cast_for_comparison(left, &common)?;
        let right = cast_for_comparison(right, &common)?;
        return Ok(distinct(&left, &right)?);
    }
    if !left.data_type().is_nested()
        && let Ok(bits) = distinct(left, right)
    {
        return Ok(bits);
    }
    if let Ok(compare) = make_comparator(left.as_ref(), right.as_ref(), SortOptions::default()) {
        return Ok(BooleanArray::from(BooleanBuffer::collect_bool(
            left.len(),
            |row| !compare(row, row).is_eq(),
        )));
    }
    // Fall back to semantic equality for Arrow types without a comparator.
    let left = left.to_data();
    let right = right.to_data();
    Ok(BooleanArray::from(BooleanBuffer::collect_bool(
        left.len(),
        |row| left.slice(row, 1) != right.slice(row, 1),
    )))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Added,
    Removed,
    Modified,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Removed => "Removed",
            Self::Modified => "Modified",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RowChange {
    pub kind: ChangeKind,
    pub original_row: Option<u64>,
    pub revised_row: Option<u64>,
}

#[derive(Debug, Default)]
pub(crate) struct VersionDiff {
    pub counts: [usize; 3],
    added: Vec<u64>,
    removed: Vec<u64>,
    modified: Vec<(u64, u64)>,
}

impl VersionDiff {
    pub fn len(&self) -> usize {
        self.added.len() + self.removed.len() + self.modified.len()
    }

    pub fn row(&self, index: usize) -> Option<RowChange> {
        let (kind, original_row, revised_row) = if let Some(&row) = self.added.get(index) {
            (ChangeKind::Added, None, Some(row + 1))
        } else {
            let index = index - self.added.len();
            if let Some(&row) = self.removed.get(index) {
                (ChangeKind::Removed, Some(row + 1), None)
            } else {
                let &(left, right) = self.modified.get(index - self.removed.len())?;
                (ChangeKind::Modified, Some(left + 1), Some(right + 1))
            }
        };
        Some(RowChange {
            kind,
            original_row,
            revised_row,
        })
    }

    fn finish(&mut self, cancel: &AtomicBool) -> Result<()> {
        check_cancel(cancel)?;
        self.added.par_sort_unstable();
        check_cancel(cancel)?;
        self.removed.par_sort_unstable();
        check_cancel(cancel)?;
        self.modified.par_sort_unstable();
        self.counts = [self.added.len(), self.removed.len(), self.modified.len()];
        Ok(())
    }
}

pub(crate) struct DuplicateRows {
    // The first array is the row's position in the immutable displayed result.
    batches: Vec<RecordBatch>,
    ends: Vec<usize>,
    pub groups: u64,
}

impl DuplicateRows {
    pub fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    pub fn row(&self, index: usize) -> Option<(usize, u64)> {
        let batch = self.ends.partition_point(|&end| end <= index);
        let start = batch.checked_sub(1).map_or(0, |i| self.ends[i]);
        let row = index - start;
        let batch = self.batches.get(batch)?;
        Some((
            positions(batch.column(0)).ok()?.value(row) as usize,
            positions(batch.column(1)).ok()?.value(row),
        ))
    }
}

pub(crate) fn duplicate_keys(
    result: &QueryResult,
    keys: &[String],
    cancel: &Arc<AtomicBool>,
) -> Result<DuplicateRows> {
    check_cancel(cancel)?;
    validate_keys(keys)?;
    let names = key_names(keys.len());
    let schema = result.data.schema();
    let indices = keys
        .iter()
        .map(|name| schema.index_of(name))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        indices
            .iter()
            .all(|&index| supports_key(&result.columns[index].data_type)),
        "Choose matching scalar key fields."
    );
    let data = result.data.project_columns(&indices)?;
    let runtime = AuditRuntime::new()?;
    runtime.register("audit_source", &data, None, cancel)?;
    let list = names.join(", ");
    let grouped = format!(
        "SELECT {list}, CAST(COUNT(*) AS BIGINT UNSIGNED) AS repeats FROM audit_source GROUP BY {list} HAVING COUNT(*) > 1"
    );
    let on = join_condition(&names, "IS NOT DISTINCT FROM");
    // Retain only the duplicate groups and reuse them for the detail join.
    // This computes their count without a second full group-by of the source.
    let grouped = runtime.query(&grouped, cancel)?;
    let groups: u64 = grouped.iter().map(|batch| batch.num_rows() as u64).sum();
    let batches = if groups == 0 {
        Vec::new()
    } else {
        let schema = grouped[0].schema();
        runtime.context.register_table(
            "audit_groups",
            Arc::new(MemTable::try_new(schema, vec![grouped])?),
        )?;
        runtime.query(
            &format!("SELECT l.row_pos, r.repeats FROM audit_source l INNER JOIN audit_groups r ON {on} ORDER BY l.row_pos"),
            cancel,
        )?
    };
    let mut end = 0;
    let ends = batches
        .iter()
        .map(|batch| {
            end += batch.num_rows();
            end
        })
        .collect();
    Ok(DuplicateRows {
        batches,
        ends,
        groups,
    })
}

fn validate_keys(keys: &[String]) -> Result<()> {
    ensure!(!keys.is_empty(), "Choose at least one key field.");
    let mut seen = HashSet::new();
    ensure!(
        keys.iter().all(|key| seen.insert(key)),
        "Choose matching scalar key fields."
    );
    Ok(())
}

fn key_names(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("k{index}")).collect()
}

fn join_condition(names: &[String], op: &str) -> String {
    names
        .iter()
        .map(|name| format!("(l.{name} {op} r.{name})"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn positions(array: &dyn Array) -> Result<&UInt64Array> {
    array
        .as_any()
        .downcast_ref()
        .context("Unexpected audit row index type")
}

struct AuditRuntime {
    context: SessionContext,
    runtime: tokio::runtime::Runtime,
    workers: usize,
}

impl AuditRuntime {
    fn new() -> Result<Self> {
        let workers = std::thread::available_parallelism().map_or(1, |count| count.get());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("parquet-data-audit")
            .enable_all()
            .build()?;
        let _enter = runtime.enter();
        let environment = RuntimeEnvBuilder::new()
            .with_memory_pool(Arc::new(GreedyMemoryPool::new(
                crate::engine::workspace_bytes(workers),
            )))
            .build()?;
        let config = SessionConfig::new()
            .with_target_partitions(workers)
            .with_batch_size(BATCH_ROWS);
        let context = crate::datafusion_session::new_context(config, Arc::new(environment));
        drop(_enter);
        Ok(Self {
            context,
            runtime,
            workers,
        })
    }

    fn register(
        &self,
        name: &str,
        data: &ArrowTable,
        common_types: Option<&[DataType]>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<()> {
        let names = key_names(data.schema().fields().len());
        let fields = data
            .schema()
            .fields()
            .iter()
            .zip(names)
            .enumerate()
            .map(|(index, (field, name))| {
                let kind = common_types
                    .map_or_else(|| field.data_type().clone(), |types| types[index].clone());
                Field::new(name, kind, field.is_nullable())
            })
            .chain([Field::new("row_pos", DataType::UInt64, false)])
            .collect::<Vec<_>>();
        let schema = Arc::new(Schema::new(fields));
        let rows = data.row_count();
        let partitions = self.workers.min(rows.div_ceil(BATCH_ROWS).max(1));
        let per_partition = rows.div_ceil(partitions).max(1);
        let parts = (0..partitions)
            .map(|i| {
                Arc::new(KeyPartition {
                    data: data.clone(),
                    schema: schema.clone(),
                    rows: i * per_partition..((i + 1) * per_partition).min(rows),
                    cancel: cancel.clone(),
                }) as Arc<dyn PartitionStream>
            })
            .collect();
        self.context
            .register_table(name, Arc::new(StreamingTable::try_new(schema, parts)?))?;
        Ok(())
    }

    fn query(&self, sql: &str, cancel: &AtomicBool) -> Result<Vec<RecordBatch>> {
        let mut batches = Vec::new();
        self.query_each(sql, cancel, |batch| {
            if batch.num_rows() > 0 {
                batches.push(batch);
            }
            Ok(())
        })?;
        Ok(batches)
    }

    fn query_each(
        &self,
        sql: &str,
        cancel: &AtomicBool,
        mut visit: impl FnMut(RecordBatch) -> Result<()>,
    ) -> Result<()> {
        check_cancel(cancel)?;
        self.runtime.block_on(async {
            tokio::select! {
                result = async {
                    let frame = self.context.sql(sql).await?;
                    let mut stream = frame.execute_stream().await?;
                    while let Some(batch) = stream.try_next().await? {
                        check_cancel(cancel)?;
                        visit(batch)?;
                    }
                    check_cancel(cancel)
                } => result,
                () = async {
                    while !cancel.load(Ordering::Acquire) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                } => { check_cancel(cancel)?; unreachable!() }
            }
        })
    }
}

#[derive(Debug)]
struct KeyPartition {
    data: ArrowTable,
    schema: SchemaRef,
    rows: Range<usize>,
    cancel: Arc<AtomicBool>,
}

impl PartitionStream for KeyPartition {
    fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    fn execute(&self, _: Arc<TaskContext>) -> SendableRecordBatchStream {
        let schema = self.schema.clone();
        let data = self.data.clone();
        let rows = self.rows.clone();
        let cancel = self.cancel.clone();
        let end = rows.end;
        let stream = futures::stream::iter(rows.step_by(BATCH_ROWS).map(move |start| {
            let make = || -> Result<RecordBatch> {
                check_cancel(&cancel)?;
                let end = start.saturating_add(BATCH_ROWS).min(end);
                let batch = data.materialize_range(start..end)?;
                let mut columns = batch
                    .columns()
                    .iter()
                    .zip(schema.fields())
                    .map(|(column, field)| cast_for_comparison(column, field.data_type()))
                    .collect::<Result<Vec<_>, _>>()?;
                columns.push(Arc::new(UInt64Array::from_iter_values(
                    start as u64..end as u64,
                )));
                Ok(RecordBatch::try_new(schema.clone(), columns)?)
            };
            make().map_err(|error| datafusion::error::DataFusionError::External(error.into()))
        }));
        Box::pin(RecordBatchStreamAdapter::new(self.schema.clone(), stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int32Array, Int64Array, ListArray, StringArray};
    use arrow::datatypes::Int32Type;
    use parquet::{arrow::ArrowWriter, file::properties::WriterProperties};
    use std::{fs::File, path::Path};

    fn write(path: &Path, fields: Vec<(&str, ArrayRef)>) -> Result<()> {
        let batch = RecordBatch::try_from_iter(fields)?;
        let properties = WriterProperties::builder()
            .set_max_row_group_row_count(Some(2))
            .build();
        let mut writer =
            ArrowWriter::try_new(File::create(path)?, batch.schema(), Some(properties))?;
        writer.write(&batch)?;
        writer.close()?;
        Ok(())
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn compare_composite_keys_distinguishes_changes_and_schema_changes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let directory = std::env::var_os("PARQUET_VIEWER_VERSION_QA_DIR")
            .map_or_else(|| temporary.path().to_path_buf(), PathBuf::from);
        std::fs::create_dir_all(&directory)?;
        let old = directory.join("qa-original.parquet");
        let new = directory.join("qa-comparison.parquet");
        let list = |values| {
            Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(values)) as ArrayRef
        };
        write(
            &old,
            vec![
                ("tenant", Arc::new(Int64Array::from(vec![1, 1, 2, 4]))),
                (
                    "label",
                    Arc::new(StringArray::from(vec!["a", "b", "a", "z"])),
                ),
                (
                    "amount",
                    Arc::new(Int64Array::from(vec![Some(100), None, Some(10), Some(99)])),
                ),
                (
                    "items",
                    list(vec![
                        Some(vec![Some(1), None]),
                        None,
                        Some(vec![Some(8)]),
                        Some(vec![]),
                    ]),
                ),
                ("counter", Arc::new(Int64Array::from(vec![9, 9, 9, 9]))),
                ("old_only", Arc::new(Int32Array::from(vec![1, 2, 3, 4]))),
            ],
        )?;
        write(
            &new,
            vec![
                ("tenant", Arc::new(Int64Array::from(vec![4, 3, 1, 1]))),
                (
                    "label",
                    Arc::new(StringArray::from(vec!["z", "c", "b", "a"])),
                ),
                ("amount", Arc::new(Int64Array::from(vec![99, 19, 42, 100]))),
                (
                    "items",
                    list(vec![
                        Some(vec![]),
                        Some(vec![Some(3)]),
                        None,
                        Some(vec![Some(2), None]),
                    ]),
                ),
                ("counter", Arc::new(UInt64Array::from(vec![9, 9, 9, 9]))),
                ("new_only", Arc::new(Int32Array::from(vec![1, 2, 3, 4]))),
            ],
        )?;
        let cancel = Arc::new(AtomicBool::new(false));
        let sources = VersionSources::open(old, new, &cancel)?;
        assert_eq!(sources.keys, names(&["tenant", "label", "amount"]));
        assert_eq!(
            sources
                .schema_changes
                .iter()
                .map(|change| change.field.as_str())
                .collect::<Vec<_>>(),
            ["amount", "counter", "old_only", "new_only"]
        );
        assert!(sources.schema_changes[2].revised.is_none());
        assert!(sources.schema_changes[3].original.is_none());
        let keys = names(&["tenant", "label"]);
        let result = sources.compare(&keys, &cancel, |_, _| {})?;
        assert_eq!(result.counts, [1, 1, 2]);
        assert_eq!(result.len(), 4);
        assert_eq!(
            result.row(0),
            Some(RowChange {
                kind: ChangeKind::Added,
                original_row: None,
                revised_row: Some(2)
            })
        );
        assert_eq!(
            result.row(1),
            Some(RowChange {
                kind: ChangeKind::Removed,
                original_row: Some(3),
                revised_row: None
            })
        );
        assert_eq!(
            result.row(2),
            Some(RowChange {
                kind: ChangeKind::Modified,
                original_row: Some(1),
                revised_row: Some(4)
            })
        );
        assert_eq!(
            result.row(3),
            Some(RowChange {
                kind: ChangeKind::Modified,
                original_row: Some(2),
                revised_row: Some(3)
            })
        );
        assert!(result.row(4).is_none());
        let cancelled = sources.compare(&keys, &cancel, |done, _| {
            if done == 0 {
                cancel.store(true, Ordering::Release);
            }
        });
        assert_eq!(cancelled.unwrap_err().to_string(), "Operation cancelled");
        Ok(())
    }

    #[test]
    fn comparison_refuses_null_and_duplicate_keys_on_either_side() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let valid = directory.path().join("valid.parquet");
        let invalid = directory.path().join("invalid.parquet");
        let make = |path: &Path, labels: Vec<Option<&str>>| {
            write(
                path,
                vec![
                    ("tenant", Arc::new(Int64Array::from(vec![1; labels.len()]))),
                    ("label", Arc::new(StringArray::from(labels))),
                ],
            )
        };
        make(&valid, vec![Some("a"), Some("b")])?;
        let cancel = Arc::new(AtomicBool::new(false));
        let keys = names(&["tenant", "label"]);
        for labels in [vec![Some("a"), None], vec![Some("a"), Some("a")]] {
            make(&invalid, labels)?;
            let a = VersionSources::open(invalid.clone(), valid.clone(), &cancel)?;
            let b = VersionSources::open(valid.clone(), invalid.clone(), &cancel)?;
            assert_eq!(
                a.compare(&keys, &cancel, |_, _| {})
                    .unwrap_err()
                    .to_string(),
                "Original file contains NULL or duplicate keys."
            );
            assert_eq!(
                b.compare(&keys, &cancel, |_, _| {})
                    .unwrap_err()
                    .to_string(),
                "Comparison file contains NULL or duplicate keys."
            );
        }
        Ok(())
    }

    #[test]
    fn comparison_handles_empty_identical_and_disjoint_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let empty = directory.path().join("empty.parquet");
        let old = directory.path().join("old.parquet");
        let new = directory.path().join("new.parquet");
        for (file, ids) in [(&empty, vec![]), (&old, vec![1, 2]), (&new, vec![3, 4, 5])] {
            write(file, vec![("id", Arc::new(Int64Array::from(ids)))])?;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        for (original, revised, expected) in [
            (&empty, &empty, [0, 0, 0]),
            (&empty, &new, [3, 0, 0]),
            (&old, &empty, [0, 2, 0]),
            (&old, &old, [0, 0, 0]),
            (&old, &new, [3, 2, 0]),
        ] {
            let sources = VersionSources::open(original.clone(), revised.clone(), &cancel)?;
            let result = sources.compare(&names(&["id"]), &cancel, |_, _| {})?;
            assert_eq!(result.counts, expected);
        }
        Ok(())
    }

    #[test]
    fn comparison_accepts_csv_and_parquet_and_preserves_original_csv_options() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let old = directory.path().join("old.csv");
        let new = directory.path().join("new.parquet");
        std::fs::write(&old, "id|value\n101|same\n102|changed\n103|removed\n")?;
        write(
            &new,
            vec![
                ("id", Arc::new(StringArray::from(vec!["102", "101", "104"]))),
                (
                    "value",
                    Arc::new(StringArray::from(vec!["updated", "same", "added"])),
                ),
            ],
        )?;
        let cancel = Arc::new(AtomicBool::new(false));
        let options = CsvOptions {
            delimiter: b'|',
            has_header: true,
            infer_types: false,
            ..Default::default()
        };
        let sources = VersionSources::open_with_options(old, new, Some(options), &cancel)?;
        let result = sources.compare(&names(&["id"]), &cancel, |_, _| {})?;
        assert_eq!(result.counts, [1, 1, 1]);
        assert_eq!(result.added, vec![2]);
        assert_eq!(result.removed, vec![2]);
        assert_eq!(result.modified, vec![(1, 0)]);
        Ok(())
    }

    #[test]
    fn comparison_preserves_custom_text_layout_and_uses_compatible_exact_types() -> Result<()> {
        use crate::engine::csv_source::CsvColumnType;
        let directory = tempfile::tempdir()?;
        let old = directory.path().join("original.records");
        let revised = directory.path().join("revised.records");
        std::fs::write(&old, "code^id^amount\n001^1^1.25\n002^2^2.5\n003^3^3.5\n")?;
        std::fs::write(
            &revised,
            "id^code^amount\n2^002^10.125\n1^001^1.250\n4^004^4\n",
        )?;
        let options = CsvOptions {
            delimiter: b'^',
            column_types: [(0, CsvColumnType::Text)].into(),
            ..Default::default()
        };
        let parquet = directory.path().join("revised.parquet");
        write(
            &parquet,
            vec![
                ("id", Arc::new(Int32Array::from(vec![2, 1, 4]))),
                (
                    "code",
                    Arc::new(StringArray::from(vec!["002", "001", "004"])),
                ),
                (
                    "amount",
                    Arc::new(
                        arrow::array::Decimal128Array::from(vec![10125, 1250, 4000])
                            .with_precision_and_scale(20, 3)?,
                    ),
                ),
            ],
        )?;
        let cancel = Arc::new(AtomicBool::new(false));
        for path in [revised, parquet] {
            let sources = VersionSources::open_with_options(
                old.clone(),
                path,
                Some(options.clone()),
                &cancel,
            )?;
            let result = sources.compare(&names(&["id", "code"]), &cancel, |_, _| {})?;
            assert_eq!(result.counts, [1, 1, 1]);
            assert_eq!(result.modified, vec![(1, 0)]);
            assert!(
                sources
                    .schema_changes
                    .iter()
                    .any(|change| change.field == "amount")
            );
        }
        Ok(())
    }

    #[test]
    fn typed_text_result_preserves_filter_profile_and_parquet_export() -> Result<()> {
        use crate::{
            engine::{Engine, ExportFormat, QueryRequest},
            filtering::{FilterCondition, FilterGroup, FilterOperator},
            profile_stats::{ProfileDetails, ProfileMode, calculate_profile},
        };

        let directory = tempfile::tempdir()?;
        let source = directory.path().join("transactions.tsv");
        std::fs::write(
            &source,
            "date\tamount\tcode\n2024-02-28\t1.5\t001\n2024-03-01\t2.00\t002\n2024-03-02\t\t003\n",
        )?;
        let mut engine = Engine::open(&[source])?;
        let filter = FilterGroup {
            conditions: vec![FilterCondition {
                field: "date".into(),
                operator: FilterOperator::Ge,
                value: "2024-03-01".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let result = engine.query(&QueryRequest {
            filter: filter.to_filter(&engine.info.columns)?,
            ..Default::default()
        })?;
        assert_eq!(result.row_count(), 2);
        assert_eq!(result.source_row_number(0), Some(2));
        let cancel = AtomicBool::new(false);
        let profile = calculate_profile(&result, 0, ProfileMode::Auto, 0, 8, &cancel)?;
        assert!(matches!(
            profile.details,
            ProfileDetails::Temporal(Some((low, high)))
                if low == "2024-03-01" && high == "2024-03-02"
        ));
        let profile = calculate_profile(&result, 1, ProfileMode::Auto, 0, 8, &cancel)?;
        assert_eq!((profile.rows, profile.nulls), (2, 1));
        let parquet = directory.path().join("filtered.parquet");
        engine.export_result(&result.snapshot_id, &parquet, ExportFormat::Parquet)?;
        let mut restored = Engine::open(&[parquet])?;
        let restored = restored.query(&QueryRequest::default())?;
        assert_eq!(restored.data.schema(), result.data.schema());
        assert_eq!(restored.cell(0, 1).text, "2.0");
        assert!(restored.cell(1, 1).is_null);
        assert_eq!(restored.cell(1, 2).text, "003");
        Ok(())
    }

    #[test]
    fn duplicate_audit_keeps_null_composite_groups_and_current_sort_order() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("duplicates.parquet");
        write(
            &path,
            vec![
                ("id", Arc::new(Int64Array::from_iter_values(0..8))),
                (
                    "tenant",
                    Arc::new(Int64Array::from(vec![
                        Some(1),
                        Some(1),
                        Some(1),
                        None,
                        None,
                        None,
                        Some(1),
                        Some(3),
                    ])),
                ),
                (
                    "label",
                    Arc::new(StringArray::from(vec![
                        None,
                        None,
                        Some("x"),
                        Some("k"),
                        Some("k"),
                        None,
                        Some("x"),
                        Some("unique"),
                    ])),
                ),
            ],
        )?;
        let mut engine = crate::engine::Engine::open(&[path])?;
        let result = engine.query(&crate::engine::QueryRequest {
            sort: Some(("id".into(), true)),
            ..Default::default()
        })?;
        let cancel = Arc::new(AtomicBool::new(false));
        let keys = names(&["tenant", "label"]);
        let duplicates = duplicate_keys(&result, &keys, &cancel)?;
        assert_eq!(duplicates.groups, 3);
        assert_eq!(duplicates.len(), 6);
        let rows = (0..duplicates.len())
            .map(|index| {
                let (position, count) = duplicates.row(index).unwrap();
                (result.source_row_number(position).unwrap(), count)
            })
            .collect::<Vec<_>>();
        assert_eq!(rows, [(7, 2), (5, 2), (4, 2), (3, 2), (2, 2), (1, 2)]);
        assert!(duplicates.row(duplicates.len()).is_none());
        assert_eq!(duplicate_keys(&result, &names(&["id"]), &cancel)?.len(), 0);
        cancel.store(true, Ordering::Release);
        assert!(duplicate_keys(&result, &keys, &cancel).is_err());
        Ok(())
    }

    #[test]
    fn duplicate_float_keys_follow_arrow_total_order() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("float-duplicates.parquet");
        write(
            &path,
            vec![(
                "key",
                Arc::new(Float64Array::from(vec![-0., 0., f64::NAN, f64::NAN])),
            )],
        )?;
        let mut engine = crate::engine::Engine::open(&[path])?;
        let result = engine.query(&Default::default())?;
        let cancel = Arc::new(AtomicBool::new(false));
        let duplicates = duplicate_keys(&result, &names(&["key"]), &cancel)?;
        assert_eq!(duplicates.groups, 1);
        assert_eq!(duplicates.len(), 2);
        assert_eq!(duplicates.row(0), Some((2, 2)));
        assert_eq!(duplicates.row(1), Some((3, 2)));
        Ok(())
    }
}
