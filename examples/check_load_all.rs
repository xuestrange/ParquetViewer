//! Import every row into owned Arrow RAM and verify progress and snapshot safety.
//! Run against an existing fixture, or derive a bounded prefix without modifying
//! the original: check_load_all SOURCE.parquet 150000 PREFIX.parquet
#![allow(dead_code)]
#[path = "../src/aggregation.rs"]
mod aggregation;
#[path = "../src/columnar.rs"]
mod columnar;
#[path = "../src/datafusion_session.rs"]
mod datafusion_session;
#[path = "../src/engine.rs"]
mod engine;
#[path = "../src/resource_budget.rs"]
mod resource_budget;
#[path = "../src/source_guard.rs"]
mod source_guard;

#[path = "../src/arrow_types.rs"]
mod arrow_types;
#[path = "../src/export.rs"]
mod export;
#[path = "../src/filtering.rs"]
mod filtering;
#[path = "../src/nested.rs"]
mod nested;
#[path = "../src/parquet_compat.rs"]
mod parquet_compat;
#[path = "../src/source_loader.rs"]
mod source_loader;
mod support;
#[cfg(test)]
#[path = "../src/test_support.rs"]
mod test_support;
use anyhow::{Context, Result, ensure};
use engine::{Engine, ExportFormat, QueryPhase, QueryProgress, QueryRequest};
use std::{
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let original = PathBuf::from(
        arguments
            .first()
            .context("Usage: check_load_all SOURCE [PREFIX_ROWS PREFIX_DESTINATION]")?,
    );
    let original_metadata = std::fs::metadata(&original)?;
    source_guard::register_sources(std::slice::from_ref(&original))?;
    let path = if let Some(rows) = arguments.get(1) {
        let rows: usize = rows.to_str().context("Invalid row count")?.parse()?;
        ensure!(rows > 0, "Prefix row count must be positive");
        let output = PathBuf::from(arguments.get(2).context("Prefix destination is required")?);
        ensure!(
            !output.exists(),
            "Refusing to overwrite an existing prefix fixture"
        );
        std::fs::create_dir_all(output.parent().context("Prefix needs a parent directory")?)?;
        source_guard::guard_destination(&output, std::slice::from_ref(&original))?;
        support::copy_prefix(&original, &output, rows)?;
        output
    } else {
        original.clone()
    };
    let cancel = AtomicBool::new(false);
    let mut engine = Engine::open_cancellable(std::slice::from_ref(&path), &cancel)?;
    ensure!(
        engine.info.row_count_known && engine.info.row_count > 0,
        "Expected a nonempty Parquet fixture with exact row metadata"
    );
    let total = engine.info.row_count;
    let previous = engine.query(&QueryRequest {
        limit: 100,
        ..Default::default()
    })?;
    let progress = QueryProgress::default();
    let observer = progress.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let monitor = std::thread::spawn(move || {
        let mut samples = Vec::new();
        while !stopping.load(Ordering::Acquire) {
            let snapshot = observer.snapshot();
            samples.push(serde_json::json!({
                "phase":format!("{:?}",snapshot.phase), "fraction":snapshot.fraction,
                "processed_rows":snapshot.processed_rows,"total_rows":snapshot.total_rows
            }));
            std::thread::sleep(Duration::from_millis(20));
        }
        samples
    });
    let started = Instant::now();
    let result = engine.query_cancellable_with_progress(
        &QueryRequest {
            limit: total,
            ..Default::default()
        },
        &cancel,
        &progress,
    );
    stop.store(true, Ordering::Release);
    let samples = monitor.join().unwrap();
    let result = result?;
    let load_ms = started.elapsed().as_millis();
    let timings = serde_json::json!({
        "snapshot_ms":result.timings.snapshot_ms,"metadata_ms":result.timings.metadata_ms,
        "arrow_start_ms":result.timings.arrow_start_ms,"arrow_fetch_ms":result.timings.arrow_fetch_ms,
        "adapter_ms":result.timings.adapter_ms
    });
    ensure!(
        result.row_count() == total,
        "Load All did not import every fixture row"
    );
    ensure!(
        result.data.memory_size() > 128 * 1024 * 1024,
        "Fixture does not exercise the former 128 MiB limit"
    );
    let id = result.columns.iter().position(|column| column.name == "id");
    let nested = result
        .columns
        .iter()
        .enumerate()
        .find_map(|(column, _)| result.is_nested(0, column).then_some(column));
    let mut anchors = Vec::new();
    for row in [0, total / 2, total - 1] {
        if let Some(id) = id {
            ensure!(
                result.cell(row, id).text == row.to_string(),
                "Sequential fixture anchor mismatch at {row}"
            );
        }
        if let Some(column) = nested {
            ensure!(
                result.cell(row, column).json.is_some(),
                "Nested typed value missing at {row}"
            );
        }
        anchors.push(serde_json::json!({"row":row,"id_checked":id.is_some(),"nested_checked":nested.is_some()}));
    }
    let final_progress = progress.snapshot();
    ensure!(
        final_progress.phase == QueryPhase::Complete
            && final_progress.processed_rows == total
            && final_progress.fraction == Some(1.0),
        "Final progress is not exact"
    );
    let retained_column = nested.unwrap_or(0);
    let expected = result.cell(total - 1, retained_column);
    let retained = result
        .slice_cell(total - 1, retained_column)
        .context("Missing retained slice")?;
    let logical_bytes = result.data.memory_size();
    let large_snapshot = result.snapshot_id.clone();
    drop(result);
    engine.discard_snapshot(&large_snapshot)?;

    // Cancel another real load after work has started, without losing the prior
    // successful immutable snapshot. One observer polls actual engine phases.
    let cancel_progress = QueryProgress::default();
    let observer = cancel_progress.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    let done = Arc::new(AtomicBool::new(false));
    let completed = done.clone();
    let ready = Arc::new(Barrier::new(2));
    let watching = ready.clone();
    let canceller = std::thread::spawn(move || {
        watching.wait();
        while !completed.load(Ordering::Acquire) {
            let state = observer.snapshot();
            // The direct Arrow path enters Preparing before fetching its first
            // batch. Wait for actual processed rows in either path so this
            // validates cancellation after work, rather than pre-start cancel.
            if matches!(state.phase, QueryPhase::Preparing | QueryPhase::Loading)
                && state.processed_rows > 0
            {
                signal.store(true, Ordering::Release);
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    });
    ready.wait();
    let cancelled = engine.query_cancellable_with_progress(
        &QueryRequest {
            limit: total,
            ..Default::default()
        },
        &cancel,
        &cancel_progress,
    );
    done.store(true, Ordering::Release);
    let signaled = canceller.join().unwrap();
    ensure!(
        signaled && cancelled.is_err(),
        "Expected cooperative cancellation after processing began; signaled={signaled}, final_progress={:?}, result={:?}",
        cancel_progress.snapshot(),
        cancelled
            .as_ref()
            .map(|value| value.row_count())
            .map_err(|error| error.to_string())
    );
    let output = path
        .parent()
        .context("Fixture needs a parent directory")?
        .join(format!("load-all-retained-{}.parquet", std::process::id()));
    engine.export_result_columns_cancellable(
        &previous.snapshot_id,
        &[],
        &output,
        ExportFormat::Parquet,
        &AtomicBool::new(false),
    )?;
    drop(engine);
    let after = retained.cell(0, 0);
    ensure!(
        after.text == expected.text && after.json == expected.json,
        "Retained Peek changed after Engine/result drop"
    );
    ensure!(
        std::fs::metadata(&original)?.modified()? == original_metadata.modified()?
            && std::fs::metadata(&original)?.len() == original_metadata.len(),
        "Original fixture metadata changed"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "source":original.canonicalize()?, "loaded_fixture":path.canonicalize()?,
            "all_rows":total,"arrow_owned_bytes":logical_bytes,"load_all_ms":load_ms,
            "query_timings":timings,
            "progress_samples":samples,"final_fraction":final_progress.fraction,
            "anchors":anchors,"cancelled_after_work_started":true,
            "previous_snapshot_export_after_cancel":output.canonicalize()?,
            "retained_slice_after_engine_drop":true,"source_size_mtime_unchanged":true,
            "scope":"Every fixture row imported into owned Arrow RAM; logical Arrow bytes are not process RSS."
        }))?
    );
    Ok(())
}
