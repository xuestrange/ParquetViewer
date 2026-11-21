//! Typed aggregation correctness and repeatable, projection-aware timings.
//! With no arguments, creates target/demo/aggregation.parquet (200,000 rows)
//! and validates real Engine grouping/filtering/NULL/Decimal operations. --make
//! only creates the fixture for GUI checks. SOURCE [GROUP_FIELD] benchmarks an
//! existing imported source with its first integer field as the metric; sources
//! are read-only and no full-file hash is included inside operation timings.
#![allow(dead_code)]
#[path = "../src/aggregation.rs"]
mod aggregation;
#[path = "../src/arrow_types.rs"]
mod arrow_types;
#[path = "../src/columnar.rs"]
mod columnar;
#[path = "../src/datafusion_session.rs"]
mod datafusion_session;
#[path = "../src/engine.rs"]
mod engine;
#[path = "../src/export.rs"]
mod export;
#[path = "../src/filtering.rs"]
mod filtering;
#[path = "../src/nested.rs"]
mod nested;
#[path = "../src/parquet_compat.rs"]
mod parquet_compat;
#[path = "../src/resource_budget.rs"]
mod resource_budget;
#[path = "../src/source_guard.rs"]
mod source_guard;
#[path = "../src/source_loader.rs"]
mod source_loader;
#[cfg(test)]
#[path = "../src/test_support.rs"]
mod test_support;

use aggregation::{AggregateMetric, AggregateOp as Op, AggregateRequest};
use anyhow::{Context, Result, ensure};
use arrow::{
    array::{
        ArrayRef, BooleanArray, Date32Array, Decimal128Array, Float64Array, Int64Array, ListArray,
        StringArray, StructArray,
    },
    datatypes::{DataType, Field, Int64Type},
    record_batch::RecordBatch,
};
use engine::{Engine, QueryRequest};
use serde_json::json;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

const FIXTURE_ROWS: usize = 200_000;

fn make_fixture(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path.parent().context("Fixture needs a parent")?)?;
    let category = Arc::new(StringArray::from_iter((0..FIXTURE_ROWS).map(|i| {
        if i % 10 == 9 {
            None
        } else {
            Some(["华东", "华南", "华北"][i % 3])
        }
    }))) as ArrayRef;
    let id = Arc::new(Int64Array::from_iter_values(
        (0..FIXTURE_ROWS).map(|i| i as i64),
    ));
    let amount = Arc::new(
        Decimal128Array::from(
            (0..FIXTURE_ROWS)
                .map(|i| (i % 11 != 0).then_some(i as i128 * 100 + 25))
                .collect::<Vec<_>>(),
        )
        .with_precision_and_scale(20, 2)?,
    ) as ArrayRef;
    let text = Arc::new(StringArray::from_iter(
        (0..FIXTURE_ROWS).map(|i| (i % 7 != 0).then(|| format!("item-{}", i % 17))),
    )) as ArrayRef;
    let flag = Arc::new(BooleanArray::from(
        (0..FIXTURE_ROWS)
            .map(|i| (i % 5 != 0).then_some(i % 2 == 0))
            .collect::<Vec<_>>(),
    )) as ArrayRef;
    let day = Arc::new(Date32Array::from(
        (0..FIXTURE_ROWS)
            .map(|i| (i % 13 != 0).then_some(20000 + (i % 31) as i32))
            .collect::<Vec<_>>(),
    )) as ArrayRef;
    let score = Arc::new(Float64Array::from_iter_values(
        (0..FIXTURE_ROWS).map(|i| (i % 101) as f64 / 8.),
    )) as ArrayRef;
    let nested = Arc::new(StructArray::from(vec![(
        Arc::new(Field::new("id", DataType::Int64, false)),
        id.clone() as ArrayRef,
    )])) as ArrayRef;
    let list = Arc::new(ListArray::from_iter_primitive::<Int64Type, _, _>(
        (0..FIXTURE_ROWS).map(|i| Some(vec![Some(i as i64), None])),
    )) as ArrayRef;
    let batch = RecordBatch::try_from_iter(vec![
        ("category", category),
        ("id", id as ArrayRef),
        ("amount", amount),
        ("text", text),
        ("flag", flag),
        ("day", day),
        ("score", score),
        ("struct", nested),
        ("list", list),
    ])?;
    let properties = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(32768))
        .build();
    let mut writer = parquet::arrow::ArrowWriter::try_new(
        File::create(path)?,
        batch.schema(),
        Some(properties),
    )?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

fn field(name: &str, op: Op) -> AggregateMetric {
    AggregateMetric {
        field: Some(name.into()),
        op,
    }
}

fn main() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let make_only = args.first().is_some_and(|arg| arg == "--make");
    let own = args.is_empty() || make_only;
    let source = if own {
        PathBuf::from("target/demo/aggregation.parquet")
    } else {
        PathBuf::from(&args[0])
    };
    if own {
        make_fixture(&source)?;
    }
    if make_only {
        println!(
            "{}",
            json!({"fixture":source.canonicalize()?,"rows":FIXTURE_ROWS})
        );
        return Ok(());
    }
    let before = fs::metadata(&source)?;
    let at = Instant::now();
    let mut engine = Engine::open(std::slice::from_ref(&source))?;
    let open_ms = at.elapsed().as_secs_f64() * 1000.;
    let total = engine.info.row_count;
    let at = Instant::now();
    let imported = engine.query(&QueryRequest {
        limit: total.max(1),
        ..Default::default()
    })?;
    let load_ms = at.elapsed().as_secs_f64() * 1000.;
    let (groups, metric) = if own {
        (vec!["category".to_owned()], "id".to_owned())
    } else {
        let metric = engine
            .info
            .columns
            .iter()
            .find(|column| {
                matches!(
                    column.data_type.as_str(),
                    "INT64" | "INTEGER" | "UBIGINT" | "UINTEGER"
                )
            })
            .context("Source needs an integer metric")?
            .name
            .clone();
        let groups = args
            .get(1)
            .map(|arg| vec![arg.to_string_lossy().into_owned()])
            .unwrap_or_default();
        (groups, metric)
    };
    let mut request = AggregateRequest {
        group_by: groups,
        group_buckets: Vec::new(),
        metrics: vec![
            AggregateMetric {
                field: None,
                op: Op::CountRows,
            },
            field(&metric, Op::Sum),
            field(&metric, Op::Mean),
            field(&metric, Op::NUnique),
        ],
    };
    if own {
        request.metrics.extend([
            field("amount", Op::Sum),
            field("text", Op::Count),
            field("text", Op::NUnique),
            field("day", Op::Min),
            field("day", Op::Max),
            field("flag", Op::NUnique),
        ]);
    }
    let mut trials = Vec::new();
    for trial in 0..3 {
        let at = Instant::now();
        let result = engine.aggregate_snapshot_cancellable(
            &imported.snapshot_id,
            &request,
            &AtomicBool::new(false),
        )?;
        let elapsed_ms = at.elapsed().as_secs_f64() * 1000.;
        ensure!(result.input_rows == total, "Wrong input row count");
        if own {
            ensure!(
                total == FIXTURE_ROWS && result.data.row_count() == 4,
                "Wrong fixture/group count"
            );
            let mut expected = std::collections::BTreeMap::<Option<&str>, (usize, i128)>::new();
            for i in 0..FIXTURE_ROWS {
                let key = if i % 10 == 9 {
                    None
                } else {
                    Some(["华东", "华南", "华北"][i % 3])
                };
                let item = expected.entry(key).or_default();
                item.0 += 1;
                item.1 += i as i128;
            }
            for row in 0..result.data.row_count() {
                let group = result.data.cell(row, 0);
                let key = if group.is_null {
                    None
                } else {
                    Some(group.text.as_str())
                };
                let expected = expected.get(&key).context("Unexpected group")?;
                ensure!(
                    result.data.cell(row, 1).text == expected.0.to_string(),
                    "Count rows changed"
                );
                ensure!(
                    result.data.cell(row, 2).text == expected.1.to_string(),
                    "Exact sum changed"
                );
                ensure!(
                    result.data.cell(row, 4).text == expected.0.to_string(),
                    "Exact NUnique id changed"
                );
                ensure!(
                    result.data.cell(row, 7).text == "17",
                    "Text NUnique changed"
                );
            }
        }
        let anchors = (0..result.data.row_count().min(8))
            .map(|row| {
                (0..result.columns.len())
                    .map(|column| result.data.cell(row, column).text)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        trials.push(json!({"trial":trial+1,"milliseconds":elapsed_ms,"engine_elapsed_ms":result.elapsed_ms,"input_rows":result.input_rows,"groups":result.data.row_count(),"columns":result.columns.iter().map(|column|json!({"name":column.name,"type":column.data_type})).collect::<Vec<_>>(),"anchors":anchors,"result_logical_arrow_bytes":result.data.memory_size()}));
    }
    let mut filtered_check = None;
    if own {
        let displayed = engine.query(&QueryRequest {
            offset: 10,
            limit: 100,
            filter: "id >= 50".into(),
            sort: Some(("id".into(), true)),
            ..Default::default()
        })?;
        let result = engine.aggregate_snapshot_cancellable(
            &displayed.snapshot_id,
            &AggregateRequest {
                group_by: vec![],
                group_buckets: Vec::new(),
                metrics: vec![
                    AggregateMetric {
                        field: None,
                        op: Op::CountRows,
                    },
                    field("id", Op::Sum),
                ],
            },
            &AtomicBool::new(false),
        )?;
        ensure!(
            result.input_rows == 60
                && result.data.cell(0, 0).text == "60"
                && result.data.cell(0, 1).text == "4770",
            "Filtered imported range changed"
        );
        ensure!(
            displayed.cell(0, 1).text == "109" && displayed.source_row_number(0) == Some(110),
            "Aggregation changed main snapshot"
        );
        filtered_check = Some(
            json!({"input_rows":result.input_rows,"count":result.data.cell(0,0).text,"sum":result.data.cell(0,1).text,"main_first_id":109,"main_source_row":110}),
        );
    }
    let after = fs::metadata(&source)?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "Source changed"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"source":source.canonicalize()?,"source_rows":total,"source_bytes":before.len(),"open_ms":open_ms,"load_ms":load_ms,"trials":trials,"filtered_range_check":filtered_check,"source_size_mtime_unchanged":true,"scope":"Aggregation timings only; imported source remains shared Arrow RAM and main result is unchanged. First trial includes initial DF runtime/context startup; following calls still re-plan. Rust expected-value loops occur outside timed calls. Whole-process profiling additionally includes fixture creation when absent, loading and correctness validation."})
        )?
    );
    Ok(())
}
