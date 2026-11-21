//! Compare replay with a bounded projected gather on the same Arrow snapshot.
//! Preparation and typed result equality checks are outside each timing.
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

use aggregation::{AggregateMetric, AggregateOp, AggregateRequest};
use anyhow::{Result, ensure};
use arrow::{
    array::{Array, Int64Array},
    compute::concat_batches,
    datatypes::SchemaRef,
    record_batch::RecordBatch,
};
use columnar::ArrowTable;
use datafusion::{
    catalog::{TableProvider, streaming::StreamingTable},
    datasource::MemTable,
    error::DataFusionError,
    execution::{TaskContext, runtime_env::RuntimeEnv},
    physical_plan::{
        SendableRecordBatchStream, stream::RecordBatchStreamAdapter, streaming::PartitionStream,
    },
    prelude::SessionConfig,
};
use engine::{Engine, QueryRequest};
use serde_json::json;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

const GATHER_BATCH_ROWS: usize = 8192;

#[derive(Debug)]
struct GatherPartition {
    projected: ArrowTable,
    schema: SchemaRef,
    rows: Range<usize>,
}

impl PartitionStream for GatherPartition {
    fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    fn execute(&self, _: Arc<TaskContext>) -> SendableRecordBatchStream {
        let projected = self.projected.clone();
        let end = self.rows.end;
        let stream = futures::stream::iter(self.rows.clone().step_by(GATHER_BATCH_ROWS).map(
            move |start| {
                projected
                    .materialize_range(start..(start + GATHER_BATCH_ROWS).min(end))
                    .map_err(|error| DataFusionError::External(error.into()))
            },
        ));
        Box::pin(RecordBatchStreamAdapter::new(self.schema.clone(), stream))
    }
}

async fn measure(
    data: &ArrowTable,
    plan: &aggregation::AggregatePlan,
    filter: &str,
    workers: usize,
    gather: bool,
) -> Result<(f64, RecordBatch)> {
    let start = Instant::now();
    let context = datafusion_session::new_context(
        SessionConfig::new()
            .with_target_partitions(workers)
            .with_batch_size(32768),
        Arc::new(RuntimeEnv::default()),
    );
    let provider: Arc<dyn TableProvider> = if gather {
        let schema = data.schema();
        let projected =
            data.project_columns(&[schema.index_of("category")?, schema.index_of("id")?])?;
        let schema = projected.schema();
        let rows = projected.row_count();
        let batches = rows.div_ceil(GATHER_BATCH_ROWS);
        let per_partition = GATHER_BATCH_ROWS * batches.div_ceil(workers);
        let partitions = (0..rows)
            .step_by(per_partition.max(1))
            .map(|start| {
                Arc::new(GatherPartition {
                    projected: projected.clone(),
                    schema: schema.clone(),
                    rows: start..(start + per_partition).min(rows),
                }) as Arc<dyn PartitionStream>
            })
            .collect();
        Arc::new(StreamingTable::try_new(schema, partitions)?)
    } else {
        let batches = data.raw_batches();
        let per_partition = batches.len().div_ceil(workers).max(1);
        let partitions = batches
            .chunks(per_partition)
            .map(<[RecordBatch]>::to_vec)
            .collect();
        Arc::new(MemTable::try_new(data.schema(), partitions)?)
    };
    context.register_table("aggregation_input", provider)?;
    let frame = context
        .sql(&plan.sql(if gather { "" } else { filter }))
        .await?;
    let schema = Arc::new(frame.schema().as_arrow().clone());
    let batches = frame.collect().await?;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.;
    Ok((elapsed_ms, concat_batches(&schema, &batches)?))
}

fn high_cardinality(path: &Path, mut engine: Engine) -> Result<()> {
    let rows = engine.info.row_count;
    let snapshot = engine.query(&QueryRequest {
        limit: rows,
        ..Default::default()
    })?;
    ensure!(snapshot.row_count() == rows && rows > 0);
    let category = snapshot.data.schema().index_of("category")?;
    let request = AggregateRequest {
        group_by: vec!["id".into()],
        group_buckets: Vec::new(),
        metrics: vec![
            AggregateMetric {
                field: None,
                op: AggregateOp::CountRows,
            },
            AggregateMetric {
                field: Some("category".into()),
                op: AggregateOp::NUnique,
            },
        ],
    };
    let cancel = AtomicBool::new(false);
    let mut trials = Vec::new();
    for trial in 0..4 {
        let start = Instant::now();
        let result =
            engine.aggregate_snapshot_cancellable(&snapshot.snapshot_id, &request, &cancel)?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.;
        ensure!(result.input_rows == rows && result.data.row_count() == rows);
        let mut position = 0;
        for batch in result.data.raw_batches() {
            let int64 = |column: usize| -> Result<&Int64Array> {
                let values = batch
                    .column(column)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or_else(|| anyhow::anyhow!("Expected Int64 aggregate column {column}"))?;
                ensure!(values.null_count() == 0);
                Ok(values)
            };
            let ids = int64(0)?;
            let counts = int64(1)?;
            let uniques = int64(2)?;
            for row in 0..batch.num_rows() {
                ensure!(ids.value(row) == i64::try_from(position)?);
                ensure!(counts.value(row) == 1);
                ensure!(
                    uniques.value(row) == i64::from(!snapshot.data.is_null(position, category))
                );
                position += 1;
            }
        }
        ensure!(position == rows);
        trials.push(json!({
            "trial": trial + 1, "cold_query_runtime": trial == 0,
            "elapsed_ms": elapsed_ms, "groups": result.data.row_count(),
            "result_logical_arrow_bytes": result.data.memory_size(),
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "source": path.canonicalize()?, "source_rows": rows, "trials": trials,
            "all_sorted_keys_count_and_exact_nunique_verified": true,
            "scope": "Engine aggregation of unique id groups with Count rows and exact NUnique(category). First trial includes lazy Tokio runtime initialization. All trials include session construction, plan, execution and result adaptation. Input load and typed value checks are outside timing. Logical result Arrow bytes are separate from whole-process peak RSS.",
        }))?
    );
    Ok(())
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    let high = first
        .as_deref()
        .is_some_and(|value| value == "--high-cardinality");
    let source = if high { arguments.next() } else { first };
    let path = source.map_or_else(
        || PathBuf::from("target/demo/aggregation.parquet"),
        PathBuf::from,
    );
    let mut engine = Engine::open(std::slice::from_ref(&path))?;
    if high {
        return high_cardinality(&path, engine);
    }
    let rows = engine.info.row_count;
    let workers = std::thread::available_parallelism()?.get();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
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
    let mut reports = Vec::new();
    for (case, filter, reverse) in [
        (
            "sparse_list",
            "array_has(\"list\", 101) OR array_has(\"list\", 81920)",
            false,
        ),
        ("sparse_numeric_sorted", "id % 997 = 0", true),
        ("sparse_numeric_boundary", "id % 257 = 0", false),
        ("medium_numeric", "id % 17 = 0", false),
        ("dense_numeric", "id >= 20000", false),
    ] {
        let snapshot = engine.query(&QueryRequest {
            limit: rows,
            filter: filter.into(),
            sort: reverse.then(|| ("id".into(), true)),
            ..Default::default()
        })?;
        engine.retain_snapshot(&snapshot.snapshot_id)?;
        let selected_rows = snapshot.row_count();
        ensure!(
            selected_rows > 0,
            "Benchmark filter must match at least one row: {case}"
        );
        let plan = aggregation::plan(
            &request,
            &snapshot.columns,
            &snapshot.data.schema(),
            selected_rows,
        )?;
        let data = &snapshot.data;
        let trials = runtime.block_on(async {
            let (_, expected) = measure(data, &plan, filter, workers, false).await?;
            let (_, candidate) = measure(data, &plan, filter, workers, true).await?;
            ensure!(
                expected == candidate,
                "Different typed aggregate results: {case}"
            );
            let mut replay = Vec::new();
            let mut gather = Vec::new();
            for trial in 0..7 {
                let modes = if trial % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                };
                for mode in modes {
                    let (elapsed, output) = measure(data, &plan, filter, workers, mode).await?;
                    ensure!(
                        output == expected,
                        "Different typed aggregate results: {case}"
                    );
                    if mode {
                        gather.push(elapsed);
                    } else {
                        replay.push(elapsed);
                    }
                }
            }
            Ok::<_, anyhow::Error>((replay, gather))
        })?;
        reports.push(json!({
            "case": case, "selected_rows": selected_rows, "source_rows": rows,
            "gather_ms": trials.1, "replay_ms": trials.0, "all_typed_results_equal": true,
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "source": path.canonicalize()?, "workers": workers,
            "gather_batch_rows_max": GATHER_BATCH_ROWS, "reports": reports,
            "scope": "Same loaded snapshot, one warmup per strategy, seven alternated paired trials. Session and provider construction, planning, aggregation and output collection are timed. Source load, original filter computation and typed result checks are outside timing. Prototype uses the same configuration for both paths and default RuntimeEnv, no OS memory/disk claims.",
        }))?
    );
    Ok(())
}
