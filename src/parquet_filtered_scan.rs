//! Scan the whole source, with predicate columns decoded before result columns.
//! A result owns only the selected rows and their original one-based positions.

use super::*;
use crate::columnar::{RowIndexChunk, RowIndexWidth};
use arrow::{
    array::{Array, ArrayRef, BooleanArray, UInt64Array},
    buffer::BooleanBuffer,
    compute::{filter_record_batch, kernels::filter::prep_null_mask_filter, nullif},
    record_batch::RecordBatchOptions,
};
use datafusion::{
    common::{Column, ScalarValue},
    physical_expr::{
        PhysicalExpr,
        utils::{collect_columns, reassign_expr_columns},
    },
    physical_optimizer::pruning::{PruningPredicate, PruningStatistics},
};
use parquet::{
    arrow::arrow_reader::statistics::StatisticsConverter, basic::Type as PhysicalType,
    file::metadata::RowGroupMetaData,
};

// Each selected range can generate both a select and a skip selector. Keep
// the temporary run vector bounded even for alternating matches in a huge
// row group. Such scattered matches can touch nearly every payload page.
const MAX_SELECTION_RANGES: usize = 64 * 1024;

struct GroupJob {
    partition: usize,
    group: usize,
    start: usize,
    rows: usize,
}

struct DecodedGroup {
    start: usize,
    batches: Vec<RecordBatch>,
    // Whole groups have consecutive positions. Defer allocating them until
    // we know whether the entire result can use one contiguous source range.
    positions: Option<Vec<UInt64Array>>,
}

struct Projections {
    predicate: PartitionProjection,
    output: PartitionProjection,
}

struct Decoder<'a> {
    predicate: &'a dyn PhysicalExpr,
    predicate_schema: &'a SchemaRef,
    output_schema: &'a SchemaRef,
    // If predicate decode covers all displayed fields, keep the filtered
    // batches directly instead of reading the same columns a second time.
    direct_projection: Option<Vec<usize>>,
    batch_rows: usize,
    cancel: &'a AtomicBool,
}

/// Match a filter against the resident Arrow arrays without constructing a
/// synthetic index column or a query workspace. None is the identity view.
pub(crate) fn filter_loaded_indices(
    pool: &rayon::ThreadPool,
    table: &ArrowTable,
    predicate: &dyn PhysicalExpr,
    cancel: &AtomicBool,
    on_rows: impl Fn(usize) + Sync,
) -> Result<Option<Vec<RowIndexChunk>>> {
    enum BatchMatches {
        None,
        All,
        Partial(RowIndexChunk),
    }

    check_cancel(cancel)?;
    ensure!(
        matches!(
            predicate.data_type(&table.schema())?,
            DataType::Boolean | DataType::Null
        ),
        "A source filter must return Boolean values"
    );
    let width = RowIndexWidth::for_rows(table.row_count());
    let batches = table.raw_batches();
    let examined = AtomicUsize::new(0);
    let matches = pool.install(|| {
        batches
            .par_iter()
            .enumerate()
            .map(|(index, batch)| -> Result<BatchMatches> {
                check_cancel(cancel)?;
                let mask = evaluate_mask(predicate, batch)?;
                let rows = batch.num_rows();
                let count = mask.true_count();
                let result = if count == rows {
                    BatchMatches::All
                } else if count == 0 {
                    BatchMatches::None
                } else {
                    BatchMatches::Partial(width.matching_positions(
                        table.raw_batch_start(index),
                        mask.values(),
                        count,
                    ))
                };
                check_cancel(cancel)?;
                on_rows(examined.fetch_add(rows, Ordering::AcqRel) + rows);
                Ok(result)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    check_cancel(cancel)?;
    if matches
        .iter()
        .all(|batch| matches!(batch, BatchMatches::All))
    {
        return Ok(None);
    }
    let mut positions = Vec::new();
    // The indexed parallel iterator preserves the source batch order.
    // Keep all-matching chunks as ranges, with no position buffer.
    for (index, batch) in matches.into_iter().enumerate() {
        check_cancel(cancel)?;
        match batch {
            BatchMatches::None => {}
            BatchMatches::All => {
                let start = table.raw_batch_start(index);
                let end = start + batches[index].num_rows();
                positions.push(width.contiguous_positions(start..end));
            }
            BatchMatches::Partial(chunk) => positions.push(chunk),
        }
    }
    Ok(Some(positions))
}

impl ParquetSource {
    /// Evaluate a typed predicate planned against [`Self::schema`] over the
    /// entire source. Progress counts examined rows, including pruned groups.
    pub(crate) fn load_filtered(
        &self,
        selected: &[String],
        predicate: Arc<dyn PhysicalExpr>,
        resources: &Client,
        cancel: &AtomicBool,
        on_rows: impl Fn(usize) + Sync,
    ) -> Result<ArrowTable> {
        check_cancel(cancel)?;
        let output_indices = self.selected_indices(selected)?;
        let output_schema = Arc::new(self.schema.project(&output_indices)?);
        let mut predicate_indices = collect_columns(&predicate)
            .into_iter()
            .map(|column| self.schema.index_of(column.name()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        predicate_indices.sort_unstable();
        predicate_indices.dedup();
        let predicate_schema = Arc::new(self.schema.project(&predicate_indices)?);
        let projected_predicate = reassign_expr_columns(predicate.clone(), &predicate_schema)?;
        ensure!(
            matches!(
                projected_predicate.data_type(&predicate_schema)?,
                DataType::Boolean | DataType::Null
            ),
            "A source filter must return Boolean values"
        );

        self.validate_scan_source(cancel)?;
        if predicate_indices.is_empty() {
            let unit = RecordBatch::try_new_with_options(
                predicate_schema,
                vec![],
                &RecordBatchOptions::new().with_row_count(Some(1)),
            )?;
            if evaluate_mask(projected_predicate.as_ref(), &unit)?.value(0)
                && self.info.row_count != 0
            {
                let result = self.load_window(
                    selected,
                    0,
                    self.info.row_count,
                    resources,
                    cancel,
                    &on_rows,
                )?;
                self.validate_scan_source(cancel)?;
                return Ok(result);
            }
            on_rows(self.info.row_count);
            self.validate_scan_source(cancel)?;
            return ArrowTable::new_with_schema(
                columns_from_schema(&output_schema),
                vec![],
                output_schema,
            );
        }

        let pruning = PruningPredicate::try_new(predicate, self.schema.clone())
            .ok()
            .filter(|predicate| !predicate.always_true());
        let mut jobs = Vec::new();
        let mut projections = Vec::with_capacity(self.partitions.len());
        let mut skipped = 0usize;
        for (partition_index, partition) in self.partitions.iter().enumerate() {
            check_cancel(cancel)?;
            let keep = pruning.as_ref().and_then(|predicate| {
                predicate
                    .prune(&GroupStatistics {
                        partition,
                        schema: &self.schema,
                    })
                    .ok()
            });
            let mut start = partition.start;
            for (group, metadata) in partition.reader.metadata().row_groups().iter().enumerate() {
                check_cancel(cancel)?;
                let rows = usize::try_from(metadata.num_rows())?;
                if rows != 0 {
                    if keep.as_ref().and_then(|groups| groups.get(group)) == Some(&false) {
                        skipped = skipped
                            .checked_add(rows)
                            .context("Parquet row group range overflow")?;
                    } else {
                        jobs.push(GroupJob {
                            partition: partition_index,
                            group,
                            start,
                            rows,
                        });
                    }
                }
                start = start
                    .checked_add(rows)
                    .context("Parquet row group range overflow")?;
            }
            ensure!(
                start == partition.start + partition.rows,
                "Parquet row group counts differ from the file footer"
            );
            projections.push(Projections {
                predicate: PartitionProjection::new(partition, &predicate_indices),
                output: PartitionProjection::new(partition, &output_indices),
            });
        }

        let decoder = Decoder {
            predicate: projected_predicate.as_ref(),
            predicate_schema: &predicate_schema,
            output_schema: &output_schema,
            direct_projection: output_indices
                .iter()
                .map(|index| predicate_indices.binary_search(index).ok())
                .collect(),
            batch_rows: self.batch_rows(),
            cancel,
        };
        let examined = AtomicUsize::new(skipped);
        if skipped != 0 {
            on_rows(skipped);
        }
        let groups = self.pool.install(|| {
            jobs.par_iter()
                .map(|job| {
                    let partition = &self.partitions[job.partition];
                    decoder.decode(partition, &projections[job.partition], job, |rows| {
                        on_rows(examined.fetch_add(rows, Ordering::AcqRel) + rows);
                    })
                })
                .collect::<Result<Vec<_>>>()
        })?;
        self.validate_scan_source(cancel)?;
        ensure!(
            examined.load(Ordering::Acquire) == self.info.row_count,
            "Parquet reader did not examine the complete source"
        );

        let complete = groups
            .iter()
            .flat_map(|group| &group.batches)
            .map(RecordBatch::num_rows)
            .sum::<usize>()
            == self.info.row_count;
        let mut batches = Vec::new();
        let mut positions = Vec::new();
        for group in groups {
            check_cancel(cancel)?;
            if !complete {
                if let Some(numbers) = group.positions {
                    positions.extend(numbers);
                } else {
                    let mut start = group.start as u64 + 1;
                    for batch in &group.batches {
                        let end = start + batch.num_rows() as u64;
                        positions.push(UInt64Array::from_iter_values(start..end));
                        start = end;
                    }
                }
            }
            batches.extend(group.batches);
        }
        let table = ArrowTable::new_with_schema(
            columns_from_schema(&output_schema),
            batches,
            output_schema,
        )?;
        let mut table = if complete {
            table.with_source_row_start(1)
        } else {
            table.with_source_row_numbers(positions)?
        };
        let lease = resources.reserve_display(0)?;
        lease.grow_to(table.memory_size(), cancel)?;
        table.retain_memory(lease);
        check_cancel(cancel)?;
        Ok(table)
    }

    fn validate_scan_source(&self, cancel: &AtomicBool) -> Result<()> {
        for partition in &self.partitions {
            check_cancel(cancel)?;
            ensure!(
                Stamp::read(&partition.path)? == partition.stamp,
                "Source changed after loading. Reload the file."
            );
        }
        check_cancel(cancel)
    }
}

impl Decoder<'_> {
    fn decode(
        &self,
        partition: &Partition,
        projection: &Projections,
        job: &GroupJob,
        on_rows: impl Fn(usize),
    ) -> Result<DecodedGroup> {
        check_cancel(self.cancel)?;
        let file = File::open(&partition.path)?;
        let mut reader = ParquetRecordBatchReaderBuilder::new_with_metadata(
            file.try_clone()?,
            partition.reader.clone(),
        )
        .with_row_groups(vec![job.group])
        .with_projection(projection.predicate.mask.clone())
        .with_batch_size(self.batch_rows)
        .build()?;
        let mut masks = Vec::new();
        let mut batches = Vec::new();
        let mut examined = 0usize;
        let mut selected = 0usize;
        let mut first_match = None;
        let mut pending_progress = 0;
        loop {
            check_cancel(self.cancel)?;
            let Some(batch) = reader.next() else { break };
            let batch = projection.predicate.adapt(batch?, self.predicate_schema)?;
            let mask = evaluate_mask(self.predicate, &batch)?;
            let rows = batch.num_rows();
            let matches = mask.true_count();
            if matches != 0 && first_match.is_none() {
                first_match = mask.values().set_indices().next().map(|row| examined + row);
            }
            examined += rows;
            selected += matches;
            ensure!(
                examined <= job.rows,
                "Parquet predicate reader exceeded its row group"
            );
            if matches != 0
                && let Some(columns) = &self.direct_projection
            {
                let batch = batch.project(columns)?;
                batches.push(if matches == rows {
                    batch
                } else {
                    filter_record_batch(&batch, &mask)?
                });
            }
            masks.push(mask);
            if examined == job.rows {
                // Leave the group's last progress increment pending until
                // its projected output is also ready.
                pending_progress = rows;
            } else {
                on_rows(rows);
            }
        }
        drop(reader);
        check_cancel(self.cancel)?;
        ensure!(
            examined == job.rows,
            "Parquet predicate reader returned an incomplete row group"
        );
        if selected != 0 && self.direct_projection.is_none() {
            let partial = selected < job.rows;
            let fragmented = partial
                && masks
                    .iter()
                    .flat_map(|mask| mask.values().set_slices())
                    .nth(MAX_SELECTION_RANGES)
                    .is_some();
            let indexed = if partial && !fragmented {
                first_match.and_then(|skip| {
                    parquet_page_index::for_range(
                        &file,
                        &partition.reader,
                        job.group,
                        skip,
                        selected,
                    )
                })
            } else {
                None
            };
            let (metadata, group) = indexed.map_or_else(
                || (partition.reader.clone(), job.group),
                |metadata| (metadata, 0),
            );
            let mut builder = ParquetRecordBatchReaderBuilder::new_with_metadata(file, metadata)
                .with_row_groups(vec![group])
                .with_projection(projection.output.mask.clone())
                .with_batch_size(self.batch_rows);
            if partial && !fragmented {
                builder = builder.with_row_selection(RowSelection::from_filters(&masks));
            }
            let mut reader = builder.build()?;
            let mut masks = masks.iter();
            loop {
                check_cancel(self.cancel)?;
                let Some(batch) = reader.next() else { break };
                let mut batch = batch?;
                if fragmented {
                    let mask = masks
                        .next()
                        .context("Parquet output exceeded its predicate batches")?;
                    ensure!(
                        mask.len() == batch.num_rows(),
                        "Parquet output and predicate batch boundaries differ"
                    );
                    match mask.true_count() {
                        0 => continue,
                        rows if rows < mask.len() => batch = filter_record_batch(&batch, mask)?,
                        _ => {}
                    }
                }
                batches.push(projection.output.adapt(batch, self.output_schema)?);
            }
            ensure!(
                !fragmented || masks.next().is_none(),
                "Parquet output ended before its predicate batches"
            );
        }
        ensure!(
            batches.iter().map(RecordBatch::num_rows).sum::<usize>() == selected,
            "Parquet result row count differs from the filter"
        );

        let positions = if selected != 0 && selected != job.rows {
            let mut indices = masks
                .iter()
                .scan(job.start as u64 + 1, |start, mask| {
                    let offset = *start;
                    *start += mask.len() as u64;
                    Some(
                        mask.values()
                            .set_indices()
                            .map(move |row| offset + row as u64),
                    )
                })
                .flatten();
            let mut output = Vec::with_capacity(batches.len());
            for batch in &batches {
                check_cancel(self.cancel)?;
                let mut rows = Vec::with_capacity(batch.num_rows());
                rows.extend(indices.by_ref().take(batch.num_rows()));
                output.push(UInt64Array::from(rows));
            }
            Some(output)
        } else {
            None
        };
        on_rows(pending_progress);
        Ok(DecodedGroup {
            start: job.start,
            batches,
            positions,
        })
    }
}

fn evaluate_mask(predicate: &dyn PhysicalExpr, batch: &RecordBatch) -> Result<BooleanArray> {
    let result = predicate.evaluate(batch)?.into_array(batch.num_rows())?;
    if result.data_type() == &DataType::Null {
        return Ok(BooleanArray::new(
            BooleanBuffer::new_unset(batch.num_rows()),
            None,
        ));
    }
    let mask = result
        .as_any()
        .downcast_ref::<BooleanArray>()
        .context("A source filter must return Boolean values")?;
    ensure!(
        mask.len() == batch.num_rows(),
        "Source filter returned a different number of rows"
    );
    // SQL WHERE only retains true. RowSelection requires a non-null bitmap.
    Ok(if mask.null_count() == 0 {
        mask.clone()
    } else {
        prep_null_mask_filter(mask)
    })
}

struct GroupStatistics<'a> {
    partition: &'a Partition,
    schema: &'a SchemaRef,
}

impl GroupStatistics<'_> {
    fn groups(&self) -> &[RowGroupMetaData] {
        self.partition.reader.metadata().row_groups()
    }

    fn converter(&self, column: &str) -> Option<StatisticsConverter<'_>> {
        let physical_schema = self.partition.reader.schema();
        let source = physical_schema.field_with_name(column).ok()?;
        let target = self.schema.field_with_name(column).ok()?;
        // First support exact scalar bounds with an unchanged type. Floats
        // have NaN ordering subtleties and partition casts can change bounds.
        if source.data_type() != target.data_type()
            || !(target.data_type().is_integer()
                || matches!(
                    target.data_type(),
                    DataType::Boolean
                        | DataType::Date32
                        | DataType::Timestamp(_, _)
                        | DataType::Utf8
                        | DataType::LargeUtf8
                        | DataType::Utf8View
                ))
        {
            return None;
        }
        let parquet_schema = self.partition.reader.parquet_schema();
        let converter =
            StatisticsConverter::try_new(column, physical_schema, parquet_schema).ok()?;
        let leaf = converter.parquet_column_index()?;
        if parquet_schema.column(leaf).physical_type() == PhysicalType::INT96 {
            return None;
        }
        Some(converter.with_missing_null_counts_as_zero(false))
    }

    fn bounds(&self, column: &Column, minimum: bool) -> Option<ArrayRef> {
        let converter = self.converter(&column.name)?;
        let leaf = converter.parquet_column_index()?;
        let invalid = BooleanArray::from_iter(self.groups().iter().map(|group| {
            Some(group.column(leaf).statistics().is_none_or(|stats| {
                stats.is_min_max_deprecated()
                    || if minimum {
                        !stats.min_is_exact()
                    } else {
                        !stats.max_is_exact()
                    }
            }))
        }));
        let values = if minimum {
            converter.row_group_mins(self.groups().iter())
        } else {
            converter.row_group_maxes(self.groups().iter())
        }
        .ok()?;
        nullif(values.as_ref(), &invalid).ok()
    }
}

impl PruningStatistics for GroupStatistics<'_> {
    fn min_values(&self, column: &Column) -> Option<ArrayRef> {
        self.bounds(column, true)
    }

    fn max_values(&self, column: &Column) -> Option<ArrayRef> {
        self.bounds(column, false)
    }

    fn num_containers(&self) -> usize {
        self.groups().len()
    }

    fn null_counts(&self, column: &Column) -> Option<ArrayRef> {
        let leaf = self.converter(&column.name)?.parquet_column_index()?;
        Some(Arc::new(UInt64Array::from_iter(self.groups().iter().map(
            |group| {
                let rows = u64::try_from(group.num_rows()).ok()?;
                group
                    .column(leaf)
                    .statistics()?
                    .null_count_opt()
                    .filter(|&nulls| nulls <= rows)
            },
        ))))
    }

    fn row_counts(&self) -> Option<ArrayRef> {
        Some(Arc::new(UInt64Array::from_iter(
            self.groups()
                .iter()
                .map(|group| u64::try_from(group.num_rows()).ok()),
        )))
    }

    fn contained(&self, _column: &Column, _values: &HashSet<ScalarValue>) -> Option<BooleanArray> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::{
        array::{BinaryArray, FixedSizeBinaryArray, Int64Array, ListArray, StringArray},
        datatypes::Int64Type,
    };
    use datafusion::{
        common::DFSchema, execution::runtime_env::RuntimeEnv, prelude::SessionConfig,
    };
    use parquet::{arrow::ArrowWriter, file::properties::WriterProperties};

    fn plan(source: &ParquetSource, filter: &str) -> Result<Arc<dyn PhysicalExpr>> {
        let context = crate::datafusion_session::new_context(
            SessionConfig::new(),
            Arc::new(RuntimeEnv::default()),
        );
        let schema = DFSchema::try_from(source.schema())?;
        Ok(context.create_physical_expr(context.parse_sql_expr(filter, &schema)?, &schema)?)
    }

    fn write_partition(path: &Path, start: i64, fixed_payload: bool) -> Result<()> {
        let values = start..start + 12;
        let id = Arc::new(Int64Array::from_iter_values(values.clone())) as ArrayRef;
        let label = Arc::new(StringArray::from_iter_values(
            values.clone().map(|id| format!("记录{id}")),
        )) as ArrayRef;
        let items = Arc::new(ListArray::from_iter_primitive::<Int64Type, _, _>(
            values
                .clone()
                .map(|id| (id % 5 != 0).then_some([Some(id % 3), None])),
        )) as ArrayRef;
        let flag = Arc::new(BooleanArray::from_iter(
            values
                .clone()
                .map(|id| (id % 7 != 0).then_some(id % 4 != 0)),
        )) as ArrayRef;
        let bytes = values.map(|id| [id as u8; 32]);
        let payload: ArrayRef = if fixed_payload {
            Arc::new(FixedSizeBinaryArray::try_from_iter(bytes)?)
        } else {
            Arc::new(BinaryArray::from_iter_values(bytes))
        };
        let batch = RecordBatch::try_from_iter([
            ("id", id),
            ("label", label),
            ("items", items),
            ("flag", flag),
            ("payload", payload),
        ])?;
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(3))
            .build();
        let mut writer = ArrowWriter::try_new(File::create(path)?, batch.schema(), Some(props))?;
        writer.write(&batch)?;
        writer.close()?;
        Ok(())
    }

    #[test]
    fn whole_source_matches_full_read_and_retains_partition_positions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let first = directory.path().join("a.parquet");
        let second = directory.path().join("b.parquet");
        write_partition(&first, 0, true)?;
        write_partition(&second, 12, false)?;
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[second, first], &cancel)?;
        source.set_test_batch_rows(2);
        let predicate = plan(
            &source,
            "id >= 1 AND array_has(items, CAST(1 AS BIGINT)) AND flag",
        )?;
        let resources = Client::new();
        let progress = AtomicUsize::new(0);
        let actual = source.load_filtered(
            &["payload".into(), "label".into()],
            predicate.clone(),
            &resources,
            &cancel,
            |rows| {
                assert!(rows <= 24);
                progress.fetch_max(rows, Ordering::Relaxed);
            },
        )?;
        assert_eq!(progress.load(Ordering::Relaxed), 24);
        let original_rows = [2, 14, 20, 23];
        assert_eq!(actual.row_count(), original_rows.len());
        for (position, original) in original_rows.into_iter().enumerate() {
            assert_eq!(actual.source_row_number(position), Some(original));
            assert_eq!(
                actual.binary(position, 0).unwrap(),
                &[original as u8 - 1; 32]
            );
        }
        let full = source.load_window(&[], 0, 24, &resources, &cancel, |_| {})?;
        let expected = full
            .raw_batches()
            .iter()
            .map(|batch| -> Result<RecordBatch> {
                let mask = evaluate_mask(predicate.as_ref(), batch)?;
                Ok(filter_record_batch(batch, &mask)?.project(&[4, 1])?)
            })
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            arrow::compute::concat_batches(&actual.schema(), actual.raw_batches())?,
            arrow::compute::concat_batches(&actual.schema(), &expected)?,
        );

        // Requesting only a condition column exercises the single-pass path.
        let ids = source.load_filtered(&["id".into()], predicate, &resources, &cancel, |_| {})?;
        assert_eq!(ids.row_count(), 4);
        assert_eq!(ids.cell(3, 0).text, "22");
        assert_eq!(ids.source_row_number(3), Some(23));
        let merged = actual.merge_columns(&ids)?;
        assert_eq!(merged.source_row_number(2), Some(20));
        assert_eq!(merged.cell(2, 2).text, "19");
        Ok(())
    }

    #[test]
    fn whole_source_skips_rejected_groups_and_cancels_between_batches() -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("data.parquet");
        let valid = directory.path().join("valid.parquet");
        write_partition(&path, 0, false)?;
        write_partition(&valid, 0, false)?;
        let mut file = File::options().read(true).write(true).open(&path)?;
        let metadata = crate::parquet_compat::load_metadata(&file)?;
        let data_page = metadata
            .metadata()
            .row_group(0)
            .column(0)
            .data_page_offset();
        file.seek(SeekFrom::Start(u64::try_from(data_page)?))?;
        file.write_all(&[0xff; 8])?;
        drop(file);

        let cancel = AtomicBool::new(false);
        let resources = Client::new();
        let source = ParquetSource::open(&[path], &cancel)?;
        let data = source.load_filtered(
            &["label".into()],
            plan(&source, "id >= 9")?,
            &resources,
            &cancel,
            |_| {},
        )?;
        assert_eq!(data.row_count(), 3);
        assert_eq!(data.source_row_number(0), Some(10));
        assert_eq!(data.cell(0, 0).text, "记录9");

        let source = ParquetSource::open(&[valid], &cancel)?;
        source.set_test_batch_rows(2);
        let predicate = plan(&source, "id >= 0")?;
        let observed = AtomicUsize::new(0);
        let result =
            source.load_filtered(&["label".into()], predicate, &resources, &cancel, |rows| {
                observed.fetch_max(rows, Ordering::Relaxed);
                cancel.store(true, Ordering::Release);
            });
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(observed.load(Ordering::Relaxed) > 0);
        assert!(observed.load(Ordering::Relaxed) < 12);
        cancel.store(false, Ordering::Release);
        let empty = source.load_filtered(
            &["label".into()],
            plan(&source, "id < 0")?,
            &resources,
            &cancel,
            |_| {},
        )?;
        assert!(empty.is_empty());
        assert_eq!(empty.schema().field(0).name(), "label");
        let complete = source.load_filtered(
            &["label".into()],
            plan(&source, "true")?,
            &resources,
            &cancel,
            |_| {},
        )?;
        assert_eq!(complete.row_count(), 12);
        assert_eq!(complete.source_row_number(11), Some(12));
        assert!(
            source
                .load_filtered(
                    &["missing".into()],
                    plan(&source, "true")?,
                    &resources,
                    &cancel,
                    |_| {}
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn whole_source_fragmented_matches_keep_values_and_source_positions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("alternating.parquet");
        let rows = 2 * (MAX_SELECTION_RANGES + 1);
        let batch = RecordBatch::try_from_iter([
            (
                "id",
                Arc::new(Int64Array::from_iter_values(0..rows as i64)) as ArrayRef,
            ),
            (
                "keep",
                Arc::new(BooleanArray::from_iter(
                    (0..rows).map(|row| Some(row % 2 == 0)),
                )) as ArrayRef,
            ),
        ])?;
        let mut writer = ArrowWriter::try_new(File::create(&path)?, batch.schema(), None)?;
        writer.write(&batch)?;
        writer.close()?;
        let cancel = AtomicBool::new(false);
        let source = ParquetSource::open(&[path], &cancel)?;
        assert_eq!(source.info.row_groups, 1);
        source.set_test_batch_rows(1003);
        let data = source.load_filtered(
            &["id".into()],
            plan(&source, "keep")?,
            &Client::new(),
            &cancel,
            |_| {},
        )?;
        assert_eq!(data.row_count(), rows / 2);
        let ids = data.raw_batches().iter().flat_map(|batch| {
            batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values()
                .iter()
                .copied()
        });
        assert!(ids.eq((0..rows as i64).step_by(2)));
        for row in [0, 501, 502, data.row_count() - 1] {
            assert_eq!(data.source_row_number(row), Some(2 * row + 1));
        }
        Ok(())
    }
}
