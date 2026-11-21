//! Reproducible typed Arrow storage/correctness check using the real Engine.
//! Usage: cargo run --example check_arrow_storage -- [new-output-directory]
//! Timings are one local run, not GUI FPS, peak RSS, or a before/after benchmark.
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
#[cfg(test)]
#[path = "../src/test_support.rs"]
mod test_support;
use anyhow::{Context, Result, ensure};
use engine::{Engine, ExportFormat, QueryRequest, QueryResult};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier, atomic::AtomicBool},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const LARGE_INTEGER_BASE: i128 = 90_071_992_547_409_931_234_567_890_123_456_789;

fn sha256(path: &Path) -> Result<String> {
    let output = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()?;
    ensure!(
        output.status.success(),
        "SHA-256 failed for {}",
        path.display()
    );
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .context("Missing SHA-256")?
        .to_owned())
}

fn open(path: &Path) -> Result<(Engine, u128)> {
    let start = Instant::now();
    let engine = Engine::open_cancellable(&[path.to_owned()], &AtomicBool::new(false))?;
    Ok((engine, start.elapsed().as_millis()))
}

fn query(engine: &mut Engine, request: QueryRequest) -> Result<(QueryResult, u128)> {
    let start = Instant::now();
    let result = engine.query_cancellable(&request, &AtomicBool::new(false))?;
    Ok((result, start.elapsed().as_millis()))
}

fn null_checks(result: &QueryResult) -> Result<Value> {
    ensure!(
        result.row_count() == 100_000 && result.columns.len() == 8,
        "NULL fixture shape mismatch"
    );
    let mut probes = Vec::new();
    for row in [0, 50_000, 99_999] {
        ensure!(
            result.cell(row, 0).text == row.to_string(),
            "NULL fixture row identity changed"
        );
        probes.push(
            json!({"row":row,"id":result.cell(row,0).text,"literal":result.cell(row,7).text}),
        );
    }
    let mut nulls = BTreeMap::new();
    for (col, divisor) in [(1, 20), (2, 23), (3, 29), (4, 31), (5, 37), (6, 41)] {
        let actual = (0..result.row_count())
            .filter(|row| result.is_null(*row, col))
            .count();
        let expected = 100_000 - 100_000usize.div_ceil(divisor);
        ensure!(
            actual == expected,
            "NULL bitmap mismatch for {}",
            result.columns[col].name
        );
        nulls.insert(result.columns[col].name.clone(), actual);
    }
    for (row, expected) in [(0, ""), (1, "NULL"), (2, "NaN"), (3, "null")] {
        ensure!(
            !result.is_null(row, 7) && result.cell(row, 7).text == expected,
            "Literal was confused with SQL NULL"
        );
    }
    ensure!(
        result.cell(0, 4).json == Some(json!([0, null, 1])),
        "Nested list NULL changed"
    );
    let details = result.cell(0, 5).json.context("Missing nested struct")?;
    ensure!(
        details["records"][1].is_null() && details["records"][2]["value"].is_null(),
        "Nested NULL record/value changed"
    );
    Ok(
        json!({"anchors":probes,"top_level_nulls_by_column":nulls,"top_level_nulls_total":nulls.values().sum::<usize>(),"literal_NULL_NaN_empty_are_not_null":true,"nested_null_record_and_value_preserved":true}),
    )
}

fn wide_checks(result: &QueryResult, expected_rows: usize) -> Result<Value> {
    ensure!(
        result.row_count() == expected_rows && result.columns.len() == 1_000,
        "Wide fixture shape mismatch"
    );
    let mut probes = Vec::new();
    for row in [0, expected_rows / 2, expected_rows - 1] {
        let middle = format!("MIDDLE · row={row} · col_0500");
        let last = format!("LAST · row={row} · col_0999");
        ensure!(
            result.cell(row, 0).text == row.to_string()
                && result.cell(row, 500).text == middle
                && result.cell(row, 999).text == last,
            "Wide first/middle/last column anchors changed"
        );
        if !(row + 1).is_multiple_of(23) {
            ensure!(
                result.cell(row, 1).text
                    == format!(
                        "{}.123456789012340001",
                        9_007_199_254_740_993u64 + row as u64
                    ),
                "DECIMAL(38,18) precision changed"
            );
        }
        if !(row + 250).is_multiple_of(23) {
            ensure!(
                result.cell(row, 250).text == (LARGE_INTEGER_BASE + row as i128).to_string(),
                "Large DECIMAL integer precision changed"
            );
        }
        probes.push(json!({"row":row,"first":result.cell(row,0).text,"middle":middle,"last":last,"decimal":result.cell(row,1).text,"large_integer":result.cell(row,250).text}));
    }
    ensure!(
        result.is_null(22, 1) && !result.is_null(0, 1),
        "Wide NULL bitmap changed"
    );
    ensure!(
        result.cell(1, 49).json == Some(json!([1, 49, null, 2])),
        "Wide nested precision/NULL changed"
    );
    ensure!(
        !result.is_null(0, 4) && result.cell(0, 4).text.is_empty(),
        "Wide empty string became NULL"
    );
    Ok(
        json!({"anchors":probes,"decimal_and_beyond_u64_integer_exact":true,"null_bitmap_and_empty_string_distinct":true,"nested_list_preserved":true}),
    )
}

fn image_check(path: &Path, require_nested: bool) -> Result<Value> {
    let (mut engine, _) = open(path)?;
    let (result, _) = query(&mut engine, QueryRequest::default())?;
    for row in 0..result.row_count() {
        for col in 0..result.columns.len() {
            if require_nested && !result.is_nested(row, col) {
                continue;
            }
            let Some(bytes) = result.binary(row, col) else {
                continue;
            };
            if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") && !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                continue;
            }
            let preview = result.preview_cell(row, col, true);
            ensure!(
                preview.json.is_none() && preview.bytes.as_deref() == Some(bytes),
                "Image preview eagerly materialized JSON or changed bytes"
            );
            ensure!(
                preview.text.len() < 256,
                "Image preview retained a large text payload"
            );
            let slice = result
                .slice_cell(row, col)
                .context("Missing deferred image source")?;
            ensure!(
                slice
                    .binary(0, 0)
                    .is_some_and(|value| value.as_ptr() == bytes.as_ptr()),
                "Deferred image cell copied Arrow binary buffers"
            );
            return Ok(
                json!({"source":path.canonicalize()?,"field":result.columns[col].name,"sql_type":result.columns[col].data_type,"row":row,"image_bytes":bytes.len(),"preview_has_no_json":true,"preview_text_bytes":preview.text.len(),"deferred_copy_source_shares_binary_buffer":true}),
            );
        }
    }
    anyhow::bail!("No PNG/JPEG found in {}", path.display())
}

fn concurrent_old_snapshot_check(
    mut engine: Engine,
    snapshot: String,
    output: PathBuf,
    barrier: Arc<Barrier>,
    wide: bool,
) -> Result<Value> {
    let field = if wide { "col_0000" } else { "id" };
    let selected: Vec<String> = if wide {
        ["col_0000", "col_0001", "col_0250", "col_0500", "col_0999"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        ["id", "literal"].into_iter().map(str::to_owned).collect()
    };
    // Both independent engines reach this barrier before running queries.
    barrier.wait();
    let started = Instant::now();
    let (filtered, query_ms) = query(
        &mut engine,
        QueryRequest {
            selected_columns: selected.clone(),
            filter: format!("{field} >= 10 AND {field} < 20"),
            sort: Some((field.into(), true)),
            ..Default::default()
        },
    )?;
    ensure!(
        filtered.row_count() == 10
            && filtered.cell(0, 0).text == "19"
            && filtered.cell(9, 0).text == "10",
        "Concurrent independent filter result changed"
    );
    let export = output.join(if wide {
        "wide-old-snapshot.parquet"
    } else {
        "null-old-snapshot.parquet"
    });
    engine.export_result_columns_cancellable(
        &snapshot,
        &selected,
        &export,
        ExportFormat::Parquet,
        &AtomicBool::new(false),
    )?;
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        std::fs::File::open(&export)?,
    )?;
    ensure!(
        reader
            .schema()
            .fields()
            .iter()
            .map(|f| f.name())
            .eq(selected.iter()),
        "Old snapshot export columns changed"
    );
    let expected_rows = if wide { 1_000 } else { 100_000 };
    let mut count = 0;
    for batch in reader.with_batch_size(8192).build()? {
        let batch = batch?;
        for row in 0..batch.num_rows() {
            let text = arrow::util::display::array_value_to_string(batch.column(0).as_ref(), row)?;
            ensure!(
                text == count.to_string(),
                "Export followed the new sorted query instead of old snapshot"
            );
            if wide && count == 0 {
                ensure!(
                    arrow::util::display::array_value_to_string(batch.column(1).as_ref(), row)?
                        == "9007199254740993.123456789012340001"
                        && arrow::util::display::array_value_to_string(
                            batch.column(2).as_ref(),
                            row
                        )? == LARGE_INTEGER_BASE.to_string(),
                    "Export decimal precision changed"
                );
            }
            count += 1;
        }
    }
    ensure!(
        count == expected_rows,
        "Old snapshot export row count changed"
    );
    Ok(
        json!({"dataset":if wide {"wide"} else {"null_density"},"query_ms":query_ms,"query_and_export_validation_ms":started.elapsed().as_millis(),"filtered_rows":10,"filtered_first_id":"19","old_snapshot_export_rows":count,"old_snapshot_export":export,"old_snapshot_and_new_filter_independent":true}),
    )
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let output = args.next().map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(format!(
            "target/arrow-storage-check-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    });
    ensure!(
        args.next().is_none(),
        "Usage: check_arrow_storage [new-output-directory]"
    );
    source_guard::guard_destination(&output, &[])?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&output)
        .with_context(|| format!("Output directory must be new: {}", output.display()))?;
    let output = output.canonicalize()?;
    let null_path = Path::new("target/demo/null-density.parquet");
    let wide_path = Path::new("target/demo/wide-arrow.parquet");
    let protected = [
        null_path,
        wide_path,
        Path::new("target/demo/demo.parquet"),
        Path::new("target/demo/photo-ui.parquet"),
    ];
    let before = protected
        .iter()
        .map(|path| {
            Ok((
                path.to_path_buf(),
                sha256(path)?,
                fs::metadata(path)?.modified()?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let (mut null_engine, null_open_ms) = open(null_path)?;
    let (null_full, null_query_ms) = query(
        &mut null_engine,
        QueryRequest {
            limit: 100_000,
            ..Default::default()
        },
    )?;
    let null_report = json!({"source":null_path.canonicalize()?,"rows":null_full.row_count(),"columns":null_full.columns.len(),"open_ms":null_open_ms,"query_ms":null_query_ms,"engine_elapsed_ms":null_full.elapsed_ms,"arrow_array_memory_bytes":null_full.data.memory_size(),"historical_display_budget_estimate_mib":104.50,"historical_estimate_scope":"Prior Cell representation estimate from docs/data-behavior.md at commit d390c20. No process RSS or controlled comparison was measured.","checks":null_checks(&null_full)?});
    let null_snapshot = null_full.snapshot_id.clone();
    drop(null_full);

    let (mut wide_engine, wide_open_ms) = open(wide_path)?;
    let (wide_initial, wide_initial_ms) = query(&mut wide_engine, QueryRequest::default())?;
    let wide_initial_report = json!({"rows":wide_initial.row_count(),"columns":wide_initial.columns.len(),"query_ms":wide_initial_ms,"engine_elapsed_ms":wide_initial.elapsed_ms,"arrow_array_memory_bytes":wide_initial.data.memory_size(),"checks":wide_checks(&wide_initial,1_000)?});
    let wide_snapshot = wide_initial.snapshot_id.clone();
    drop(wide_initial);
    let (wide_full, wide_full_ms) = query(
        &mut wide_engine,
        QueryRequest {
            limit: 5_000,
            ..Default::default()
        },
    )?;
    let wide_full_report = json!({"rows":wide_full.row_count(),"columns":wide_full.columns.len(),"query_ms":wide_full_ms,"engine_elapsed_ms":wide_full.elapsed_ms,"arrow_array_memory_bytes":wide_full.data.memory_size(),"checks":wide_checks(&wide_full,5_000)?});
    drop(wide_full);
    let image_reports = [
        image_check(protected[2], false)?,
        image_check(protected[3], true)?,
    ];
    let barrier = Arc::new(Barrier::new(2));
    let parallel_start = Instant::now();
    let null_output = output.clone();
    let wide_output = output.clone();
    let null_barrier = barrier.clone();
    let (null_parallel, wide_parallel) = std::thread::scope(|scope| -> Result<_> {
        let null_job = scope.spawn(move || {
            concurrent_old_snapshot_check(
                null_engine,
                null_snapshot,
                null_output,
                null_barrier,
                false,
            )
        });
        let wide_job = scope.spawn(move || {
            concurrent_old_snapshot_check(wide_engine, wide_snapshot, wide_output, barrier, true)
        });
        Ok((
            null_job
                .join()
                .map_err(|_| anyhow::anyhow!("NULL worker panicked"))??,
            wide_job
                .join()
                .map_err(|_| anyhow::anyhow!("Wide worker panicked"))??,
        ))
    })?;
    let parallel_wall_ms = parallel_start.elapsed().as_millis();
    for (path, hash, modified) in &before {
        ensure!(
            sha256(path)? == *hash && fs::metadata(path)?.modified()? == *modified,
            "Source changed: {}",
            path.display()
        );
    }
    let report = json!({"scope":"One local correctness/storage run; not GUI FPS, peak RSS, or a controlled before/after speed comparison","policy":{"cpu":"Every independent Engine can use all available CPU workers","available_parallelism":std::thread::available_parallelism().map_or(1,|n|n.get()),"rust_debug_assertions":cfg!(debug_assertions),"arrow_result_limit_bytes":null,"memory_measurement":"Arrow get_array_memory_size estimates retained array buffers/metadata; not process RSS or peak physical footprint"},"null_density_full":null_report,"wide":{"source":wide_path.canonicalize()?,"open_ms":wide_open_ms,"default_window":wide_initial_report,"full_window":wide_full_report},"image_previews":image_reports,"concurrent_independent_engines":{"start":"Two OS threads synchronized at a barrier; no global operation gate","wall_ms":parallel_wall_ms,"datasets":[null_parallel,wide_parallel]},"sources_sha256_and_mtime_unchanged":true,"source_hashes":before.iter().map(|(path,hash,_)|json!({"source":path.canonicalize().unwrap_or_else(|_|path.clone()),"sha256":hash})).collect::<Vec<_>>(),"output_directory":output});
    let text = serde_json::to_string_pretty(&report)?;
    source_guard::save_copy(&output.join("report.json"), &[], text.as_bytes())?;
    println!("{text}");
    Ok(())
}
