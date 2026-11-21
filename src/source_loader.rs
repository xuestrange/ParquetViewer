//! Metadata-only discovery and ordered parallel Parquet -> owned Arrow RAM.
//! Each source owns its CPU pool. No query database, IPC cache or spill copy is
//! involved; Parquet's Arrow reader handles projection, row selection and decode.
use crate::{
    arrow_types::columns_from_schema, columnar::ArrowTable, engine::DatasetInfo,
    resource_budget::Client,
};
use anyhow::{Context, Result, ensure};
use arrow::{
    compute::{CastOptions, can_cast_types, cast_with_options},
    datatypes::{DataType, Field, FieldRef, Fields, Schema, SchemaRef, TimeUnit, UnionFields},
    record_batch::RecordBatch,
};
use parquet::arrow::{
    ProjectionMask,
    arrow_reader::{
        ArrowReaderMetadata, ParquetRecordBatchReaderBuilder, RowSelection, RowSelector,
    },
};
use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::SystemTime,
};

#[path = "parquet_filtered_scan.rs"]
mod parquet_filtered_scan;
pub(crate) use parquet_filtered_scan::filter_loaded_indices;
#[path = "parquet_page_index.rs"]
mod parquet_page_index;

const BATCH_ROWS: usize = 8192;
pub(crate) type SourceStamps = Vec<(PathBuf, Option<(SystemTime, u64)>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
}
impl Stamp {
    fn read(path: &Path) -> Result<Self> {
        let m = fs::metadata(path)?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: m.len(),
            modified: m.modified().ok(),
            #[cfg(unix)]
            identity: (m.dev(), m.ino()),
        })
    }
}
struct Partition {
    path: PathBuf,
    reader: ArrowReaderMetadata,
    roots: Vec<usize>,
    start: usize,
    rows: usize,
    stamp: Stamp,
}
pub struct ParquetSource {
    pub info: DatasetInfo,
    schema: SchemaRef,
    partitions: Vec<Partition>,
    pool: rayon::ThreadPool,
    #[cfg(test)]
    batch_rows: AtomicUsize,
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    Ok(())
}

impl ParquetSource {
    pub fn open(inputs: &[PathBuf], cancel: &AtomicBool) -> Result<Self> {
        check_cancel(cancel)?;
        ensure!(!inputs.is_empty(), "Choose a Parquet file.");
        let paths = discover_files(inputs, cancel)?;
        crate::source_guard::register_sources(&paths)?;
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|i| format!("parquet-decode-{i}"))
            .build()?;
        let readers = pool.install(|| {
            paths
                .par_iter()
                .map(|path| -> Result<_> {
                    check_cancel(cancel)?;
                    let stamp = Stamp::read(path)?;
                    let file = File::open(path)
                        .with_context(|| format!("Could not open {}", path.display()))?;
                    let reader = crate::parquet_compat::load_metadata(&file)
                        .with_context(|| format!("Could not read {} as Parquet", path.display()))?;
                    ensure!(
                        Stamp::read(path)? == stamp,
                        "Source changed while reading metadata. Reload the file."
                    );
                    Ok((reader, stamp))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut info = DatasetInfo {
            paths: paths.clone(),
            file_count: paths.len(),
            row_count_known: true,
            ..Default::default()
        };
        let mut partitions = Vec::with_capacity(paths.len());
        let mut schema: Option<Schema> = None;
        for (path, (reader, stamp)) in paths.into_iter().zip(readers) {
            check_cancel(cancel)?;
            let raw = reader.schema();
            let mut occurrences = HashMap::new();
            for f in raw.fields() {
                *occurrences.entry(f.name().to_uppercase()).or_insert(0usize) += 1;
            }
            let mut roots = Vec::new();
            let mut fields = Vec::new();
            for (i, f) in raw.fields().iter().enumerate() {
                if occurrences[&f.name().to_uppercase()] > 1 {
                    if !info.excluded_fields.contains(f.name()) {
                        info.excluded_fields.push(f.name().clone());
                    }
                } else {
                    roots.push(i);
                    fields.push(f.clone());
                }
            }
            ensure!(
                !fields.is_empty(),
                "No loadable fields remain in {}: all fields conflict ignoring case. Case-sensitive duplicate fields cannot be loaded.",
                path.display()
            );
            let current = Schema::new_with_metadata(fields, raw.metadata().clone());
            if let Some(previous) = &mut schema {
                *previous = merge_partition_schema(previous, &current).with_context(|| format!(
                    "Different Parquet schemas were found. {} does not match the first file. Open matching partitions together.", path.display()
                ))?;
            } else {
                schema = Some(current);
            }
            let meta = reader.metadata();
            let rows = usize::try_from(meta.file_metadata().num_rows())?;
            partitions.push(Partition {
                path,
                reader,
                roots,
                start: info.row_count,
                rows,
                stamp: stamp.clone(),
            });
            info.row_count = info
                .row_count
                .checked_add(rows)
                .context("Dataset row count exceeds addressable memory")?;
            info.row_groups = info.row_groups.saturating_add(
                partitions
                    .last()
                    .unwrap()
                    .reader
                    .metadata()
                    .num_row_groups(),
            );
            info.file_size = info.file_size.saturating_add(stamp.len);
        }
        let schema = Arc::new(schema.context("No Parquet schema was found")?);
        info.columns = columns_from_schema(&schema);
        if !info.excluded_fields.is_empty() {
            info.warnings.push(format!("The following case-insensitive duplicate fields were excluded: {}. Other fields were loaded normally.",info.excluded_fields.join(", ")));
        }
        Ok(Self {
            info,
            schema,
            partitions,
            pool,
            #[cfg(test)]
            batch_rows: AtomicUsize::new(BATCH_ROWS),
        })
    }
    #[cfg(test)]
    pub(crate) fn set_test_batch_rows(&self, rows: usize) {
        self.batch_rows.store(rows.max(1), Ordering::Release);
    }
    pub(crate) fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    pub(crate) fn pool(&self) -> &rayon::ThreadPool {
        &self.pool
    }

    pub(crate) fn file_stamps(&self, limit: usize) -> SourceStamps {
        self.partitions
            .iter()
            .take(limit)
            .map(|partition| {
                (
                    partition.path.clone(),
                    partition
                        .stamp
                        .modified
                        .map(|time| (time, partition.stamp.len)),
                )
            })
            .collect()
    }

    fn selected_indices(&self, selected: &[String]) -> Result<Vec<usize>> {
        if selected.is_empty() {
            return Ok((0..self.schema.fields().len()).collect());
        }
        let mut seen = HashSet::new();
        selected
            .iter()
            .map(|name| {
                ensure!(seen.insert(name), "Duplicate selected column: {name}");
                self.schema
                    .index_of(name)
                    .with_context(|| format!("Unknown column: {name}"))
            })
            .collect()
    }

    fn batch_rows(&self) -> usize {
        #[cfg(not(test))]
        {
            BATCH_ROWS
        }
        #[cfg(test)]
        {
            self.batch_rows.load(Ordering::Acquire)
        }
    }
    pub fn validate_window(&self, offset: usize, limit: usize) -> Result<()> {
        let end = offset.saturating_add(limit);
        for p in &self.partitions {
            if p.start < end && p.start.saturating_add(p.rows) > offset {
                ensure!(
                    Stamp::read(&p.path)? == p.stamp,
                    "Source changed after loading. Reload the file."
                );
            }
        }
        Ok(())
    }
    pub fn load_window(
        &self,
        selected: &[String],
        offset: usize,
        limit: usize,
        resources: &Client,
        cancel: &AtomicBool,
        on_rows: impl Fn(usize) + Sync,
    ) -> Result<ArrowTable> {
        check_cancel(cancel)?;
        ensure!(limit > 0, "Record count must be greater than zero.");
        let indices = self.selected_indices(selected)?;
        let target = Arc::new(self.schema.project(&indices)?);
        let end = offset.saturating_add(limit).min(self.info.row_count);
        let mut jobs = Vec::new();
        for (partition_index, p) in self.partitions.iter().enumerate() {
            if p.start >= end || p.start.saturating_add(p.rows) <= offset {
                continue;
            }
            let mut start = p.start;
            for (group, metadata) in p.reader.metadata().row_groups().iter().enumerate() {
                let rows = usize::try_from(metadata.num_rows())?;
                let stop = start
                    .checked_add(rows)
                    .context("Parquet row group range overflow")?;
                if start < end && stop > offset {
                    jobs.push((
                        partition_index,
                        group,
                        offset.saturating_sub(start),
                        end.min(stop) - offset.max(start),
                        rows,
                    ));
                }
                start = stop;
            }
        }
        self.validate_window(offset, limit)?;
        let batch_rows = self.batch_rows();
        let loaded = AtomicUsize::new(0);
        let groups = self.pool.install(|| {
            jobs.par_iter()
                .map(
                    |&(pi, group, skip, count, group_rows)| -> Result<Vec<RecordBatch>> {
                        check_cancel(cancel)?;
                        let p = &self.partitions[pi];
                        let file = File::open(&p.path)?;
                        let projection = PartitionProjection::new(p, &indices);
                        let selection = RowSelection::from(vec![
                            RowSelector::skip(skip),
                            RowSelector::select(count),
                            RowSelector::skip(group_rows - skip - count),
                        ]);
                        let (metadata, group) =
                            parquet_page_index::for_range(&file, &p.reader, group, skip, count)
                                .map_or_else(
                                    || (p.reader.clone(), group),
                                    |metadata| (metadata, 0),
                                );
                        let reader =
                            ParquetRecordBatchReaderBuilder::new_with_metadata(file, metadata)
                                .with_row_groups(vec![group])
                                .with_projection(projection.mask.clone())
                                .with_row_selection(selection)
                                .with_batch_size(batch_rows)
                                .build()?;
                        let mut batches = Vec::new();
                        for batch in reader {
                            check_cancel(cancel)?;
                            let batch = projection.adapt(batch?, &target)?;
                            let rows = batch.num_rows();
                            batches.push(batch);
                            on_rows(loaded.fetch_add(rows, Ordering::AcqRel) + rows);
                        }
                        check_cancel(cancel)?;
                        Ok(batches)
                    },
                )
                .collect::<Result<Vec<_>>>()
        })?;
        check_cancel(cancel)?;
        self.validate_window(offset, limit)?;
        ensure!(
            loaded.load(Ordering::Acquire) == end.saturating_sub(offset),
            "Parquet reader returned an incomplete row range."
        );
        let mut groups = groups.into_iter();
        let mut batches = groups.next().unwrap_or_default();
        batches.reserve_exact(groups.as_slice().iter().map(Vec::len).sum());
        batches.extend(groups.flatten());
        let mut data = ArrowTable::new_with_schema(columns_from_schema(&target), batches, target)?
            .with_source_row_start(offset.saturating_add(1));
        let lease = resources.reserve_display(0)?;
        lease.grow_to(data.memory_size(), cancel)?;
        data.retain_memory(lease);
        check_cancel(cancel)?;
        Ok(data)
    }
}

/// Parquet emits projected roots in physical order. Resolve the output order
/// once and use the same checked partition casts for both import modes.
struct PartitionProjection {
    mask: ProjectionMask,
    positions: Vec<usize>,
}

impl PartitionProjection {
    fn new(partition: &Partition, indices: &[usize]) -> Self {
        let mut roots = indices
            .iter()
            .map(|&i| partition.roots[i])
            .collect::<Vec<_>>();
        roots.sort_unstable();
        Self {
            mask: ProjectionMask::roots(
                partition.reader.metadata().file_metadata().schema_descr(),
                roots.iter().copied(),
            ),
            positions: indices
                .iter()
                .map(|&i| {
                    roots
                        .binary_search(&partition.roots[i])
                        .expect("selected root")
                })
                .collect(),
        }
    }

    fn adapt(&self, batch: RecordBatch, schema: &SchemaRef) -> Result<RecordBatch> {
        if batch.schema_ref() == schema
            && self
                .positions
                .iter()
                .enumerate()
                .all(|(index, &position)| index == position)
        {
            return Ok(batch);
        }
        let arrays = self
            .positions
            .iter()
            .enumerate()
            .map(|(out, &position)| {
                let array = batch.column(position);
                let wanted = schema.field(out).data_type();
                if array.data_type() == wanted {
                    return Ok(array.clone());
                }
                check_cast_range(array, wanted)?;
                // A failed conversion must surface as an error, not a new NULL.
                Ok(cast_with_options(
                    array,
                    wanted,
                    &CastOptions {
                        safe: false,
                        ..Default::default()
                    },
                )?)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(RecordBatch::try_new_with_options(
            schema.clone(),
            arrays,
            &arrow::record_batch::RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
        )?)
    }
}

/// Dataset shape is compared using real Arrow types, not display labels such
/// as TIME/FLOAT/BLOB which deliberately hide physical width and precision.
fn merge_partition_schema(left: &Schema, right: &Schema) -> Result<Schema> {
    ensure_matching_fields(left.fields(), right.fields())?;
    let mut a = Vec::with_capacity(left.fields().len());
    let mut b = Vec::with_capacity(right.fields().len());
    for (left, right) in left.fields().iter().zip(right.fields()) {
        ensure_matching_extensions(left, right)?;
        let common = merge_type(left.data_type(), right.data_type())?;
        ensure!(
            can_cast_types(left.data_type(), &common) && can_cast_types(right.data_type(), &common),
            "Arrow cannot safely adapt field {:?} from {} and {} to {}",
            left.name(),
            left.data_type(),
            right.data_type(),
            common
        );
        a.push(normalized_field(left, common.clone()));
        b.push(normalized_merge_input(left, right, common));
    }
    // Per-file metadata (pandas index ranges, descriptions, etc.) need not be
    // identical across partitions. Preserve the first schema's metadata while
    // Arrow merges field metadata and nullability.
    Ok(Schema::try_merge([Schema::new(a), Schema::new(b)])?.with_metadata(left.metadata().clone()))
}

fn check_cast_range(array: &arrow::array::ArrayRef, target: &DataType) -> Result<()> {
    if array.data_type() == &DataType::Time64(TimeUnit::Microsecond)
        && target == &DataType::Time64(TimeUnit::Nanosecond)
    {
        // Even malformed clock values must not wrap when Arrow scales i64
        // microseconds to nanoseconds. Min/max are Arrow kernels; no row loop.
        let raw = cast_with_options(
            array,
            &DataType::Int64,
            &CastOptions {
                safe: false,
                ..Default::default()
            },
        )?;
        let values = raw
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .context("Expected i64 time storage")?;
        ensure!(
            arrow::compute::min(values).is_none_or(|min| min >= i64::MIN / 1000)
                && arrow::compute::max(values).is_none_or(|max| max <= i64::MAX / 1000),
            "Time value cannot be represented exactly at nanosecond precision"
        );
    }
    let children = match target {
        DataType::Struct(fields) => fields
            .iter()
            .map(|field| field.data_type())
            .collect::<Vec<_>>(),
        DataType::List(field)
        | DataType::LargeList(field)
        | DataType::ListView(field)
        | DataType::LargeListView(field)
        | DataType::FixedSizeList(field, _)
        | DataType::Map(field, _) => vec![field.data_type()],
        DataType::Dictionary(_, value) => vec![value.as_ref()],
        DataType::Union(fields, _) => fields.iter().map(|(_, field)| field.data_type()).collect(),
        DataType::RunEndEncoded(runs, values) => vec![runs.data_type(), values.data_type()],
        _ => Vec::new(),
    };
    if !children.is_empty() {
        let data = array.to_data();
        for (child, target) in data.child_data().iter().zip(children) {
            check_cast_range(&arrow::array::make_array(child.clone()), target)?;
        }
    }
    Ok(())
}

fn ensure_matching_fields(left: &Fields, right: &Fields) -> Result<()> {
    ensure!(
        left.len() == right.len() && left.iter().zip(right).all(|(a, b)| a.name() == b.name()),
        "Parquet field names and order differ"
    );
    Ok(())
}

fn ensure_matching_extensions(left: &Field, right: &Field) -> Result<()> {
    for key in [
        "ARROW:extension:name",
        "ARROW:extension:metadata",
        "parquetviewer.logical_type",
    ] {
        ensure!(
            left.metadata().get(key) == right.metadata().get(key),
            "Arrow extension semantics differ for field {:?}: {key}",
            left.name()
        );
    }
    ensure!(
        crate::parquet_compat::is_interval_field(left)
            == crate::parquet_compat::is_interval_field(right),
        "Parquet INTERVAL storage semantics differ for field {:?}",
        left.name()
    );
    Ok(())
}

fn normalized_field(field: &Field, kind: DataType) -> Field {
    field
        .clone()
        .with_data_type(kind)
        .with_nullable(field.is_nullable() || field.data_type() == &DataType::Null)
}

fn normalized_merge_input(left: &Field, right: &Field, kind: DataType) -> Field {
    let mut field = normalized_field(right, kind);
    if crate::parquet_compat::is_interval_field(left)
        && crate::parquet_compat::is_interval_field(right)
    {
        // Both readers preserve identical physical INTERVAL triples. Their
        // old Arrow unit hints are explanatory, not different value semantics.
        // Keep the first partition's note without rejecting a lossless merge.
        let key = "parquetviewer.interval.original_arrow_type";
        let mut metadata = field.metadata().clone();
        if let Some(note) = left.metadata().get(key) {
            metadata.insert(key.into(), note.clone());
        } else {
            metadata.remove(key);
        }
        field = field.with_metadata(metadata);
    }
    field
}

fn merge_field(left: &Field, right: &Field, named: bool) -> Result<FieldRef> {
    ensure!(
        !named || left.name() == right.name(),
        "Nested field names differ"
    );
    ensure_matching_extensions(left, right)?;
    let kind = merge_type(left.data_type(), right.data_type())?;
    let mut field = normalized_field(left, kind.clone());
    field.try_merge(&normalized_merge_input(left, right, kind))?;
    Ok(Arc::new(field))
}

fn merge_type(left: &DataType, right: &DataType) -> Result<DataType> {
    use DataType::*;
    if left == right {
        return Ok(left.clone());
    }
    let time = |kind: &DataType| match kind {
        Time32(unit) | Time64(unit) => Some(*unit),
        _ => None,
    };
    if let (Some(a), Some(b)) = (time(left), time(right)) {
        let rank = |unit| match unit {
            TimeUnit::Second => 0,
            TimeUnit::Millisecond => 1,
            TimeUnit::Microsecond => 2,
            TimeUnit::Nanosecond => 3,
        };
        let unit = if rank(a) >= rank(b) { a } else { b };
        return Ok(match unit {
            TimeUnit::Second | TimeUnit::Millisecond => Time32(unit),
            _ => Time64(unit),
        });
    }
    let binary =
        |kind: &DataType| matches!(kind, Binary | LargeBinary | BinaryView | FixedSizeBinary(_));
    if binary(left) && binary(right) {
        return Ok(
            if matches!(left, LargeBinary | BinaryView) || matches!(right, LargeBinary | BinaryView)
            {
                LargeBinary
            } else {
                Binary
            },
        );
    }
    Ok(match (left, right) {
        (Null, kind) | (kind, Null) => kind.clone(),
        (Float16, Float32) | (Float32, Float16) => Float32,
        (Struct(a), Struct(b)) => {
            ensure_matching_fields(a, b)?;
            Struct(
                a.iter()
                    .zip(b)
                    .map(|(a, b)| merge_field(a, b, true))
                    .collect::<Result<Vec<_>>>()?
                    .into(),
            )
        }
        (List(a), List(b)) => List(merge_field(a, b, false)?),
        (LargeList(a), LargeList(b)) => LargeList(merge_field(a, b, false)?),
        (ListView(a), ListView(b)) => ListView(merge_field(a, b, false)?),
        (LargeListView(a), LargeListView(b)) => LargeListView(merge_field(a, b, false)?),
        (FixedSizeList(a, n), FixedSizeList(b, m)) if n == m => {
            FixedSizeList(merge_field(a, b, false)?, *n)
        }
        (Map(a, ordered), Map(b, other)) if ordered == other => {
            Map(merge_field(a, b, false)?, *ordered)
        }
        (Dictionary(ka, a), Dictionary(kb, b)) if ka == kb => {
            Dictionary(ka.clone(), Box::new(merge_type(a, b)?))
        }
        (RunEndEncoded(a, b), RunEndEncoded(c, d)) => {
            RunEndEncoded(merge_field(a, c, false)?, merge_field(b, d, false)?)
        }
        (Union(a, mode), Union(b, other)) if mode == other => {
            ensure!(
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((a, af), (b, bf))| a == b && af.name() == bf.name()),
                "Union fields differ"
            );
            Union(
                UnionFields::try_new(
                    a.iter().map(|(id, _)| id),
                    a.iter()
                        .zip(b.iter())
                        .map(|((_, a), (_, b))| merge_field(a, b, true))
                        .collect::<Result<Vec<_>>>()?,
                )?,
                *mode,
            )
        }
        _ => anyhow::bail!("Incompatible Arrow types: {left} and {right}"),
    })
}

fn discover_files(inputs: &[PathBuf], cancel: &AtomicBool) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(inputs.len());
    for input in inputs {
        check_cancel(cancel)?;
        ensure!(!input.is_dir(), "Choose a file instead of a folder.");
        ensure!(input.is_file(), "File does not exist: {}", input.display());
        paths.push(input.canonicalize()?);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::{
        array::{ArrayRef, Int64Array, StringArray},
        datatypes::{DataType, Field},
    };
    use parquet::{arrow::ArrowWriter, file::properties::WriterProperties};
    fn fixture(path: &Path, start: i64, rows: usize) -> Result<()> {
        let batch = RecordBatch::try_from_iter(vec![
            (
                "id",
                Arc::new(Int64Array::from_iter_values(start..start + rows as i64)) as ArrayRef,
            ),
            (
                "label",
                Arc::new(StringArray::from_iter_values(
                    (start..start + rows as i64).map(|i| format!("记录{i}")),
                )) as ArrayRef,
            ),
        ])?;
        let properties = WriterProperties::builder()
            .set_max_row_group_row_count(Some(3))
            .build();
        let mut writer =
            ArrowWriter::try_new(File::create(path)?, batch.schema(), Some(properties))?;
        writer.write(&batch)?;
        writer.close()?;
        Ok(())
    }
    #[test]
    fn physical_partition_types_promote_without_truncation_or_new_nulls() -> Result<()> {
        use arrow::{
            array::{
                Array, BinaryArray, FixedSizeBinaryArray, Float32Array, Time32MillisecondArray,
                Time64MicrosecondArray, Time64NanosecondArray,
            },
            compute::cast,
        };
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        let c = dir.path().join("c.parquet");
        let half = cast(&Float32Array::from(vec![1.5]), &DataType::Float16)?;
        let variants: Vec<(PathBuf, Vec<ArrayRef>)> = vec![
            (
                a.clone(),
                vec![
                    Arc::new(Time32MillisecondArray::from(vec![123])),
                    Arc::new(FixedSizeBinaryArray::try_from_iter(
                        [&[1u8, 2][..]].into_iter(),
                    )?),
                    half,
                ],
            ),
            (
                b.clone(),
                vec![
                    Arc::new(Time64MicrosecondArray::from(vec![123456])),
                    Arc::new(BinaryArray::from(vec![&[3u8, 4, 5][..]])),
                    Arc::new(Float32Array::from(vec![1.0001])),
                ],
            ),
            (
                c.clone(),
                vec![
                    Arc::new(Time64NanosecondArray::from(vec![123456789])),
                    Arc::new(BinaryArray::from(vec![&[6u8][..]])),
                    Arc::new(Float32Array::from(vec![9.25])),
                ],
            ),
        ];
        for (index, (path, arrays)) in variants.into_iter().enumerate() {
            let fields = ["clock", "raw", "number"]
                .into_iter()
                .zip(&arrays)
                .map(|(name, array)| Field::new(name, array.data_type().clone(), false))
                .collect::<Vec<_>>();
            let schema = Arc::new(Schema::new_with_metadata(
                fields,
                HashMap::from([("partition".into(), index.to_string())]),
            ));
            let batch = RecordBatch::try_new(schema.clone(), arrays)?;
            let mut writer = ArrowWriter::try_new(File::create(path)?, schema, None)?;
            writer.write(&batch)?;
            writer.close()?;
        }
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[c, b, a], &cancel)?;
        let data = source.load_window(&[], 0, 3, &Client::new(), &cancel, |_| {})?;
        assert_eq!(
            data.schema().field(0).data_type(),
            &DataType::Time64(TimeUnit::Nanosecond)
        );
        assert_eq!(data.schema().field(1).data_type(), &DataType::Binary);
        assert_eq!(data.schema().field(2).data_type(), &DataType::Float32);
        let times = data
            .raw_batches()
            .iter()
            .flat_map(|batch| {
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Time64NanosecondArray>()
                    .unwrap()
                    .values()
                    .iter()
                    .copied()
            })
            .collect::<Vec<_>>();
        assert_eq!(times, [123000000, 123456000, 123456789]);
        for (row, expected) in [&[1u8, 2][..], &[3u8, 4, 5][..], &[6u8][..]]
            .into_iter()
            .enumerate()
        {
            assert_eq!(data.binary(row, 1), Some(expected));
            assert!(!data.is_null(row, 1));
            assert_eq!(data.source_row_number(row), Some(row + 1));
        }
        let number = data.raw_batches()[1]
            .column(2)
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap();
        assert_eq!(number.value(0).to_bits(), 1.0001f32.to_bits());
        assert_eq!(number.null_count(), 0);
        Ok(())
    }

    #[test]
    fn nested_required_optional_partitions_widen_nullability_recursively() -> Result<()> {
        use arrow::{
            array::{Int32Array, Int32Builder, ListArray, MapBuilder, StringBuilder, StructArray},
            buffer::OffsetBuffer,
        };
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        for (path, optional) in [(&a, false), (&b, true)] {
            let value = if optional { None } else { Some(7) };
            let child = Arc::new(Field::new("age", DataType::Int32, optional));
            let profile: ArrayRef = Arc::new(StructArray::try_new(
                vec![child].into(),
                vec![Arc::new(Int32Array::from(vec![value]))],
                None,
            )?);
            let items: ArrayRef = Arc::new(ListArray::new(
                Arc::new(Field::new("item", DataType::Int32, optional)),
                OffsetBuffer::new(vec![0, 1].into()),
                Arc::new(Int32Array::from(vec![value])),
                None,
            ));
            let mut map = MapBuilder::new(None, StringBuilder::new(), Int32Builder::new())
                .with_values_field(Field::new("values", DataType::Int32, optional));
            map.keys().append_value("k");
            map.values().append_option(value);
            map.append(true)?;
            let map: ArrayRef = Arc::new(map.finish());
            let arrays = vec![profile, items, map];
            let schema = Arc::new(Schema::new(
                ["profile", "items", "mapping"]
                    .into_iter()
                    .zip(&arrays)
                    .map(|(name, array)| Field::new(name, array.data_type().clone(), false))
                    .collect::<Vec<_>>(),
            ));
            let batch = RecordBatch::try_new(schema.clone(), arrays)?;
            let mut writer = ArrowWriter::try_new(File::create(path)?, schema, None)?;
            writer.write(&batch)?;
            writer.close()?;
        }
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[b, a], &cancel)?;
        let data = source.load_window(&[], 0, 2, &Client::new(), &cancel, |_| {})?;
        let DataType::Struct(fields) = data.schema().field(0).data_type().clone() else {
            panic!("Struct expected")
        };
        assert!(fields[0].is_nullable());
        let DataType::List(item) = data.schema().field(1).data_type().clone() else {
            panic!("List expected")
        };
        assert!(item.is_nullable());
        assert!(!data.is_null(1, 0));
        assert!(data.cell(1, 0).json.unwrap()["age"].is_null());
        assert!(data.cell(1, 1).json.unwrap()[0].is_null());
        assert!(data.cell(1, 2).json.unwrap()["k"].is_null());
        assert_eq!(data.cell(0, 0).json.unwrap()["age"], 7);
        Ok(())
    }

    #[test]
    fn partition_extension_semantics_and_incompatible_types_are_not_coalesced() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let json = dir.path().join("json.parquet");
        let plain = dir.path().join("plain.parquet");
        let mut field = Field::new("value", DataType::Utf8, true);
        field.set_metadata(HashMap::from([(
            "ARROW:extension:name".into(),
            "arrow.json".into(),
        )]));
        for (path, field) in [
            (&json, field),
            (&plain, Field::new("value", DataType::Utf8, true)),
        ] {
            let schema = Arc::new(Schema::new(vec![field]));
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(StringArray::from(vec!["{}"]))],
            )?;
            let mut writer = ArrowWriter::try_new(File::create(path)?, schema, None)?;
            writer.write(&batch)?;
            writer.close()?;
        }
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(std::slice::from_ref(&json), &cancel)?;
        assert_eq!(source.info.columns[0].data_type, "JSON");
        assert!(ParquetSource::open(&[json, plain], &cancel).is_err());
        let decimal_a = Schema::new(vec![Field::new("value", DataType::Decimal128(20, 2), true)]);
        let decimal_b = Schema::new(vec![Field::new("value", DataType::Decimal128(20, 3), true)]);
        assert!(merge_partition_schema(&decimal_a, &decimal_b).is_err());
        Ok(())
    }

    #[test]
    fn nested_time_promotion_rejects_overflow_instead_of_wrapping() -> Result<()> {
        use arrow::array::{StructArray, Time64MicrosecondArray, Time64NanosecondArray};
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        for (path, array) in [
            (
                &a,
                Arc::new(Time64MicrosecondArray::from(vec![i64::MAX])) as ArrayRef,
            ),
            (
                &b,
                Arc::new(Time64NanosecondArray::from(vec![1])) as ArrayRef,
            ),
        ] {
            let profile: ArrayRef = Arc::new(StructArray::try_new(
                vec![Arc::new(Field::new(
                    "clock",
                    array.data_type().clone(),
                    true,
                ))]
                .into(),
                vec![array],
                None,
            )?);
            let schema = Arc::new(Schema::new(vec![Field::new(
                "profile",
                profile.data_type().clone(),
                true,
            )]));
            let batch = RecordBatch::try_new(schema.clone(), vec![profile])?;
            let mut writer = ArrowWriter::try_new(File::create(path)?, schema, None)?;
            writer.write(&batch)?;
            writer.close()?;
        }
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[a, b], &cancel)?;
        let error = source
            .load_window(&[], 0, 2, &Client::new(), &cancel, |_| {})
            .unwrap_err();
        assert!(format!("{error:#}").contains("nanosecond precision"));
        Ok(())
    }

    #[test]
    fn ordered_partition_windows_project_fields_and_keep_source_positions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        fixture(&a, 0, 5)?;
        fixture(&b, 5, 5)?;
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[b, a], &cancel)?;
        assert_eq!(source.info.row_count, 10);
        assert_eq!(source.info.row_groups, 4);
        assert_eq!(source.info.file_count, 2);
        let data = source.load_window(
            &["label".into(), "id".into()],
            2,
            7,
            &Client::new(),
            &cancel,
            |_| {},
        )?;
        assert_eq!(data.row_count(), 7);
        for row in 0..7 {
            assert_eq!(data.cell(row, 0).text, format!("记录{}", row + 2));
            assert_eq!(data.cell(row, 1).text, (row + 2).to_string());
            assert_eq!(data.source_row_number(row), Some(row + 3));
        }
        assert_eq!(data.schema().field(0).name(), "label");
        let empty = source.load_window(&["id".into()], 100, 20, &Client::new(), &cancel, |_| {})?;
        assert_eq!(empty.row_count(), 0);
        assert_eq!(empty.schema().field(0).data_type(), &DataType::Int64);
        Ok(())
    }
    #[test]
    fn source_window_skips_unrelated_missing_partitions_and_detects_touched_replacement()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        fixture(&a, 0, 3)?;
        fixture(&b, 3, 3)?;
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[a.clone(), b.clone()], &cancel)?;
        fs::remove_file(a)?;
        let data = source.load_window(&[], 3, 3, &Client::new(), &cancel, |_| {})?;
        assert_eq!(data.cell(0, 0).text, "3");
        let replacement = dir.path().join("replacement.parquet");
        fixture(&replacement, 30, 3)?;
        fs::rename(replacement, b)?;
        assert!(
            source
                .load_window(&[], 3, 3, &Client::new(), &cancel, |_| {})
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn case_collisions_are_excluded_without_aliasing_legitimate_fields() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("case.parquet");
        let batch = RecordBatch::try_from_iter(vec![
            ("Name", Arc::new(Int64Array::from(vec![1])) as ArrayRef),
            ("name", Arc::new(Int64Array::from(vec![2])) as ArrayRef),
            ("name_1", Arc::new(Int64Array::from(vec![3])) as ArrayRef),
        ])?;
        let mut writer = ArrowWriter::try_new(File::create(&path)?, batch.schema(), None)?;
        writer.write(&batch)?;
        writer.close()?;
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[path], &cancel)?;
        assert_eq!(source.info.excluded_fields, ["Name", "name"]);
        assert_eq!(source.info.columns.len(), 1);
        assert_eq!(source.info.columns[0].name, "name_1");
        assert_eq!(
            source
                .load_window(&[], 0, 10, &Client::new(), &cancel, |_| {})?
                .cell(0, 0)
                .text,
            "3"
        );
        Ok(())
    }
    #[test]
    fn cancellation_after_a_real_batch_does_not_publish_partial_data() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("cancel.parquet");
        // Multiple batches, with callback cancellation triggered by real rows.
        let batch = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(Int64Array::from_iter_values(0..25000)) as ArrayRef,
        )])?;
        let mut writer = ArrowWriter::try_new(File::create(&path)?, batch.schema(), None)?;
        writer.write(&batch)?;
        writer.close()?;
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[path], &cancel)?;
        assert!(
            source
                .load_window(&[], 0, 25000, &Client::new(), &cancel, |rows| {
                    if rows > 0 {
                        cancel.store(true, Ordering::Release);
                    }
                })
                .is_err()
        );
        assert!(cancel.load(Ordering::Acquire));
        Ok(())
    }
    #[test]
    fn custom_footer_values_do_not_affect_import_and_mismatched_partitions_fail() -> Result<()> {
        use parquet::file::metadata::KeyValue;
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![7]))])?;
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![KeyValue::new(
                "description".into(),
                Some("中文 Metadata".into()),
            )]))
            .build();
        let mut writer = ArrowWriter::try_new(File::create(&a)?, schema, Some(props))?;
        writer.write(&batch)?;
        writer.close()?;
        fixture(&b, 0, 3)?;
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(std::slice::from_ref(&a), &cancel)?;
        let data = source.load_window(&[], 0, 1, &Client::new(), &cancel, |_| {})?;
        assert_eq!(data.cell(0, 0).text, "7");
        assert_eq!(source.info.row_groups, 1);
        assert!(ParquetSource::open(&[a, b], &cancel).is_err());
        Ok(())
    }
}
