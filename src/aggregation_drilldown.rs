//! Match a summary row against a displayed Arrow view with typed, null-safe keys.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use arrow::{
    array::{Array, Scalar, UInt64Array},
    compute::{CastOptions, and, cast_with_options, is_null, kernels::cmp::not_distinct},
    datatypes::DataType,
};

use crate::{aggregation::AggregateResult, columnar::ArrowTable};

fn scalar_type(kind: &DataType) -> &DataType {
    match kind {
        DataType::Dictionary(_, values) => scalar_type(values),
        DataType::RunEndEncoded(_, values) => scalar_type(values.data_type()),
        _ => kind,
    }
}

pub(super) fn select_group(
    details: &ArrowTable,
    summary: &AggregateResult,
    group_count: usize,
    row: usize,
    cancel: &AtomicBool,
) -> Result<ArrowTable> {
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    ensure!(
        summary.input_rows == details.row_count(),
        "The displayed result changed. Run aggregation again."
    );
    ensure!(
        group_count > 0 && group_count == summary.group_count(),
        "Summary grouping fields are unavailable"
    );
    ensure!(row < summary.data.row_count(), "Summary row is unavailable");
    let schema = details.schema();
    let key_columns = summary
        .group_by
        .iter()
        .map(|column| schema.index_of(column))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let projected_indices = summary
        .group_by
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect::<HashMap<_, _>>();
    let projected = details.project_columns(&key_columns)?;
    let key_row = summary.data.materialize_range(row..row + 1)?;
    let cast = CastOptions {
        safe: false,
        ..Default::default()
    };
    let keys = summary
        .group_dimensions()
        .zip(key_row.columns())
        .map(|((name, bucket), value)| -> Result<_> {
            let index = projected_indices[name];
            let key = if value.logical_nulls().is_some_and(|valid| valid.is_null(0)) {
                None
            } else {
                let kind = if bucket.is_some() {
                    value.data_type()
                } else {
                    scalar_type(schema.field(key_columns[index]).data_type())
                };
                // DataFusion may represent group output using a different Arrow
                // encoding than the imported key column. Cast the one-row key.
                let key = if scalar_type(value.data_type()) == kind {
                    value.clone()
                } else {
                    cast_with_options(value.as_ref(), kind, &cast)?
                };
                Some(Scalar::new(key))
            };
            Ok((index, bucket, key))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut offset = 0usize;
    let mut selection = Vec::new();
    let mut selected_count = 0usize;
    for batch in projected.export_batches(32768, cancel) {
        let batch = batch?;
        let mut mask = None;
        for (index, bucket, key) in &keys {
            ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
            let array = batch.column(*index);
            let transformed;
            let array = if let Some(bucket) = bucket {
                transformed = bucket.apply(array)?;
                &transformed
            } else {
                array
            };
            let matched = match key {
                Some(key) => not_distinct(&array.as_ref(), key)?,
                None => is_null(array.as_ref())?,
            };
            let matched = match mask {
                Some(previous) => and(&previous, &matched)?,
                None => matched,
            };
            let empty = matched.true_count() == 0;
            mask = Some(matched);
            if empty {
                break;
            }
        }
        let mask = mask.context("Summary grouping fields are unavailable")?;
        let matches = mask.true_count();
        if matches > 0 {
            selected_count += matches;
            selection.push(UInt64Array::from_iter_values(
                mask.values()
                    .set_indices()
                    .map(|index| (offset + index) as u64),
            ));
        }
        offset += batch.num_rows();
    }
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    ensure!(
        selected_count > 0,
        "The summary group has no matching detail rows"
    );
    if selected_count == details.row_count() {
        return Ok(details.clone());
    }
    // Select the full detail view by compact positional indexes. Wide non-key
    // columns and original row numbering remain backed by existing arrays.
    let selected = details.select_view_rows(
        selection
            .into_iter()
            .take_while(|_| !cancel.load(Ordering::Acquire)),
    )?;
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::{
        array::{
            ArrayRef, BinaryArray, Decimal128Array, Float64Array, Int64Array, StringArray,
            StringViewArray,
        },
        record_batch::RecordBatch,
    };
    use std::sync::Arc;

    fn table(fields: Vec<(&str, ArrayRef)>) -> Result<ArrowTable> {
        let batch = RecordBatch::try_from_iter(fields)?;
        ArrowTable::new(
            crate::arrow_types::columns_from_schema(batch.schema().as_ref()),
            vec![batch],
        )
    }

    #[test]
    fn typed_group_respects_nulls_exact_decimals_and_existing_order() -> Result<()> {
        let exact = 12345678901234567890i128;
        let decimal = |values: Vec<i128>| -> Result<ArrayRef> {
            Ok(Arc::new(
                Decimal128Array::from(values).with_precision_and_scale(22, 3)?,
            ))
        };
        let base = table(vec![
            (
                "id",
                Arc::new(Int64Array::from(vec![101, 102, 103, 104, 105])),
            ),
            (
                "tag",
                Arc::new(StringArray::from(vec![
                    None,
                    Some("NULL"),
                    None,
                    None,
                    None,
                ])),
            ),
            (
                "amount",
                decimal(vec![exact, exact, exact + 1, exact, exact])?,
            ),
            (
                "opaque",
                Arc::new(BinaryArray::from(vec![b"data".as_slice(); 5])),
            ),
        ])?
        .with_source_row_start(101);
        let view = base.with_row_indices(UInt64Array::from(vec![3, 1, 2, 0]))?;
        let groups = table(vec![
            (
                "tag",
                Arc::new(StringViewArray::from(vec![None, Some("NULL")])),
            ),
            ("amount", decimal(vec![exact, exact])?),
            ("Count rows", Arc::new(Int64Array::from(vec![2, 1]))),
        ])?;
        let summary = AggregateResult {
            columns: crate::arrow_types::columns_from_schema(groups.schema().as_ref()),
            data: groups,
            group_by: vec!["tag".into(), "amount".into()],
            group_buckets: Vec::new(),
            input_rows: 4,
            elapsed_ms: 0,
        };
        let cancel = AtomicBool::new(false);
        let selected = select_group(&view, &summary, 2, 0, &cancel)?;
        assert_eq!(selected.row_count(), 2);
        assert_eq!(selected.source_row_number(0), Some(104));
        assert_eq!(selected.source_row_number(1), Some(101));
        assert!(Arc::ptr_eq(
            selected.raw_batches()[0].column(3),
            base.raw_batches()[0].column(3)
        ));
        let selected = select_group(&view, &summary, 2, 1, &cancel)?;
        assert_eq!(selected.row_count(), 1);
        assert_eq!(selected.source_row_number(0), Some(102));
        cancel.store(true, Ordering::Release);
        assert!(select_group(&view, &summary, 2, 0, &cancel).is_err());
        Ok(())
    }

    #[test]
    fn floating_keys_keep_signed_zero_and_nan_payloads_distinct() -> Result<()> {
        let nan = f64::from_bits(0x7ff8000000000001);
        let values = vec![
            Some(-0.),
            Some(0.),
            Some(f64::NAN),
            None,
            Some(nan),
            Some(nan),
        ];
        let details =
            table(vec![("key", Arc::new(Float64Array::from(values)))])?.with_source_row_start(1);
        let groups = table(vec![(
            "key",
            Arc::new(Float64Array::from(vec![Some(-0.), Some(nan), None])),
        )])?;
        let summary = AggregateResult {
            columns: crate::arrow_types::columns_from_schema(groups.schema().as_ref()),
            data: groups,
            group_by: vec!["key".into()],
            group_buckets: Vec::new(),
            input_rows: details.row_count(),
            elapsed_ms: 0,
        };
        let cancel = AtomicBool::new(false);
        for (row, expected) in [
            (0, vec![Some(1)]),
            (1, vec![Some(5), Some(6)]),
            (2, vec![Some(4)]),
        ] {
            let selected = select_group(&details, &summary, 1, row, &cancel)?;
            assert_eq!(
                (0..selected.row_count())
                    .map(|index| selected.source_row_number(index))
                    .collect::<Vec<_>>(),
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn temporal_group_drilldown_uses_same_local_week_and_date_buckets() -> Result<()> {
        use crate::{
            aggregation::{AggregateBucket, AggregateMetric, AggregateOp, AggregateRequest},
            engine::{Engine, QueryRequest},
        };
        use arrow::array::{Date32Array, TimestampMillisecondArray};
        use parquet::arrow::ArrowWriter;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("calendar-groups.parquet");
        let timestamps = [
            Some("2024-03-10T07:30:00Z"),
            Some("2024-03-10T08:30:00Z"),
            Some("2024-03-11T06:30:00Z"),
            Some("2024-03-11T07:30:00Z"),
            None,
        ]
        .into_iter()
        .map(|text| {
            text.map(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .unwrap()
                    .timestamp_millis()
            })
        })
        .collect::<Vec<_>>();
        let source = table(vec![
            ("id", Arc::new(Int64Array::from_iter_values(0..5))),
            (
                "date",
                Arc::new(Date32Array::from(vec![
                    Some(0),
                    Some(1),
                    Some(4),
                    Some(5),
                    None,
                ])),
            ),
            (
                "record.time",
                Arc::new(
                    TimestampMillisecondArray::from(timestamps)
                        .with_timezone("America/Los_Angeles"),
                ),
            ),
        ])?;
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(&path)?, source.schema(), None)?;
        writer.write(&source.raw_batches()[0])?;
        writer.close()?;

        let mut engine = Engine::open(&[path])?;
        let details = engine.query(&QueryRequest {
            filter: "id >= 1".into(),
            sort: Some(("id".into(), true)),
            ..Default::default()
        })?;
        let cancel = AtomicBool::new(false);
        let summary = engine.aggregate_snapshot_cancellable(
            &details.snapshot_id,
            &AggregateRequest {
                group_by: vec!["date".into(), "record.time".into()],
                group_buckets: vec![
                    ("date".into(), AggregateBucket::Month),
                    ("record.time".into(), AggregateBucket::Week),
                ],
                metrics: vec![AggregateMetric {
                    field: None,
                    op: AggregateOp::CountRows,
                }],
            },
            &cancel,
        )?;
        assert_eq!(summary.columns[0].name, "date (Month)");
        assert_eq!(summary.columns[1].name, "record.time (Week)");
        assert_eq!(
            summary.data.schema().field(0).data_type(),
            &DataType::Date32
        );
        assert_eq!(
            summary.data.schema().field(1).data_type(),
            source.schema().field(2).data_type()
        );
        assert_eq!(summary.data.row_count(), 3);
        for (row, expected) in [vec![Some(3), Some(2)], vec![Some(4)], vec![Some(5)]]
            .into_iter()
            .enumerate()
        {
            let selected = select_group(&details.data, &summary, 2, row, &cancel)?;
            assert_eq!(
                (0..selected.row_count())
                    .map(|index| selected.source_row_number(index))
                    .collect::<Vec<_>>(),
                expected,
            );
        }
        Ok(())
    }

    #[test]
    fn multiple_parts_of_one_date_group_and_drill_down_together() -> Result<()> {
        use crate::{
            aggregation::{AggregateBucket, AggregateMetric, AggregateOp, AggregateRequest},
            engine::{Engine, QueryRequest},
        };
        use arrow::array::{Int32Array, TimestampMillisecondArray};
        use parquet::arrow::ArrowWriter;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("calendar-parts.parquet");
        let timestamps = [
            Some("2024-02-01T07:30:00Z"), // January 31 in Los Angeles.
            Some("2024-01-01T08:30:00Z"),
            Some("2024-02-01T08:30:00Z"),
            Some("2025-01-01T08:30:00Z"),
            None,
        ]
        .into_iter()
        .map(|text| {
            text.map(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .unwrap()
                    .timestamp_millis()
            })
        })
        .collect::<Vec<_>>();
        let source = table(vec![
            (
                "record.time",
                Arc::new(
                    TimestampMillisecondArray::from(timestamps)
                        .with_timezone("America/Los_Angeles"),
                ),
            ),
            (
                "amount",
                Arc::new(Float64Array::from(vec![2., 4., 8., 16., 32.])),
            ),
        ])?;
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(&path)?, source.schema(), None)?;
        writer.write(&source.raw_batches()[0])?;
        writer.close()?;

        let mut engine = Engine::open(&[path])?;
        let details = engine.query(&QueryRequest {
            sort: Some(("amount".into(), true)),
            ..Default::default()
        })?;
        let cancel = AtomicBool::new(false);
        let summary = engine.aggregate_snapshot_cancellable(
            &details.snapshot_id,
            &AggregateRequest {
                group_by: vec!["record.time".into()],
                group_buckets: vec![
                    ("record.time".into(), AggregateBucket::MonthPart),
                    ("record.time".into(), AggregateBucket::YearPart),
                ],
                metrics: [AggregateOp::Max, AggregateOp::Min, AggregateOp::Mean]
                    .into_iter()
                    .map(|op| AggregateMetric {
                        field: Some("amount".into()),
                        op,
                    })
                    .collect(),
            },
            &cancel,
        )?;
        assert_eq!(summary.group_count(), 2);
        assert_eq!(
            summary
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            [
                "record.time (Month)",
                "record.time (Year)",
                "Maximum(amount)",
                "Minimum(amount)",
                "Mean(amount)"
            ]
        );
        assert_eq!(summary.data.row_count(), 4);
        let batch = summary.data.materialize_range(0..4)?;
        for (index, expected) in [
            [Some(1), Some(1), Some(2), None],
            [Some(2024), Some(2025), Some(2024), None],
        ]
        .into_iter()
        .enumerate()
        {
            let observed = batch
                .column(index)
                .as_any()
                .downcast_ref::<Int32Array>()
                .unwrap();
            assert!(observed.iter().eq(expected));
        }
        for (index, expected) in [[4., 16., 8., 32.], [2., 16., 8., 32.], [3., 16., 8., 32.]]
            .into_iter()
            .enumerate()
        {
            let observed = batch
                .column(index + 2)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            assert!(observed.iter().eq(expected.map(Some)));
        }
        for (row, expected) in [vec![2, 1], vec![4], vec![3], vec![5]]
            .into_iter()
            .enumerate()
        {
            let selected = select_group(&details.data, &summary, 2, row, &cancel)?;
            let positions = (0..selected.row_count())
                .map(|row| selected.source_row_number(row).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(positions, expected);
        }
        Ok(())
    }

    #[test]
    fn datafusion_groups_drill_down_into_a_filtered_sorted_import_range() -> Result<()> {
        use crate::{
            aggregation::{AggregateMetric, AggregateOp, AggregateRequest},
            engine::{Engine, ExportFormat, QueryRequest},
            export::export_snapshot,
        };
        use arrow::array::Date32Array;
        use parquet::arrow::ArrowWriter;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("groups.parquet");
        let nan = f64::from_bits(0x7ff8000000000001);
        let source = table(vec![
            ("id", Arc::new(Int64Array::from_iter_values(0..8))),
            (
                "record.day",
                Arc::new(Date32Array::from(vec![0, 0, 0, 0, 0, 1, 1, 1])),
            ),
            (
                "score",
                Arc::new(Float64Array::from(vec![
                    Some(-0.),
                    Some(-0.),
                    Some(0.),
                    Some(-0.),
                    Some(-0.),
                    Some(nan),
                    None,
                    Some(nan),
                ])),
            ),
        ])?;
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(&path)?, source.schema(), None)?;
        writer.write(&source.raw_batches()[0])?;
        writer.close()?;

        let sources = [path];
        let mut engine = Engine::open(&sources)?;
        let details = engine.query(&QueryRequest {
            filter: "id >= 2".into(),
            sort: Some(("id".into(), true)),
            offset: 1,
            limit: 6,
            ..Default::default()
        })?;
        let cancel = AtomicBool::new(false);
        let summary = engine.aggregate_snapshot_cancellable(
            &details.snapshot_id,
            &AggregateRequest {
                group_by: vec!["record.day".into(), "score".into()],
                group_buckets: Vec::new(),
                metrics: vec![AggregateMetric {
                    field: None,
                    op: AggregateOp::CountRows,
                }],
            },
            &cancel,
        )?;
        let mut checked = Vec::new();
        let summary_path = directory.path().join("summary.csv");
        export_snapshot(
            &summary.data,
            &summary.columns,
            &[],
            &sources,
            &summary_path,
            ExportFormat::Csv,
            &cancel,
        )?;
        let mut summary_reader = csv::Reader::from_path(summary_path)?;
        let exported_groups = summary_reader
            .records()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(exported_groups.len(), summary.data.row_count());
        for (row, exported) in exported_groups.iter().enumerate() {
            let selected = select_group(&details.data, &summary, 2, row, &cancel)?;
            assert_eq!(selected.row_count(), exported[2].parse::<usize>()?);
            let sources = (0..selected.row_count())
                .map(|index| selected.source_row_number(index).unwrap())
                .collect::<Vec<_>>();
            assert!(sources.windows(2).all(|rows| rows[0] > rows[1]));
            let group_path = directory.path().join("details.csv");
            export_snapshot(
                &selected,
                &details.columns,
                &["id".into()],
                &engine.info.paths,
                &group_path,
                ExportFormat::Csv,
                &cancel,
            )?;
            let mut group_reader = csv::Reader::from_path(group_path)?;
            let ids = group_reader
                .records()
                .map(|record| Ok(record?[0].parse::<usize>()? + 1))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(ids, sources);
            checked.extend(sources);
        }
        checked.sort_unstable();
        assert_eq!(checked, [3, 4, 5, 6, 7]);
        assert_eq!(
            (0..details.row_count())
                .map(|row| details.source_row_number(row).unwrap())
                .collect::<Vec<_>>(),
            [7, 6, 5, 4, 3]
        );
        Ok(())
    }
}
