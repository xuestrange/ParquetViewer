//! Run with `cargo run --release --example check_csv_import -- [--large]`.
//! The large mode temporarily writes a CSV over 5 GB. The full import retains
//! three narrow fields for the grouped summary. The whole-file filter retains
//! the wide text field only for matching records. The fixture is removed on exit.
//! Use `--browse-gib N` to exercise progressive browsing on a sparse CSV of N GiB.
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
use anyhow::{Context, Result, ensure};
use arrow::array::{Array, Int64Array};
use engine::csv_source::{CsvColumnType, CsvOptions, CsvProbe};
use engine::{Engine, ExportFormat, QueryPhase, QueryProgress, QueryRequest, QueryResult};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const WINDOW_ROWS: usize = 1000;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(20);
const CODE_CATEGORIES: usize = 1024;
const MATCH_STRIDE: usize = 997;

fn multiline(row: usize) -> bool {
    row.is_multiple_of(97)
}

fn fixture(path: &Path, rows: usize, payload_len: usize) -> Result<()> {
    let mut file = BufWriter::with_capacity(1024 * 1024, File::create(path)?);
    file.write_all(b"id,code,flag,payload,day\r\n")?;
    let padding = "x".repeat(payload_len);
    for row in 0..rows {
        let flag = match row % 3 {
            0 => "true",
            1 => "false",
            _ => "",
        };
        let code = row % CODE_CATEGORIES;
        write!(file, "{row},0{code:08},{flag},\"")?;
        if multiline(row) {
            file.write_all("中文,\"\"quoted\"\"\r\n".as_bytes())?;
        }
        writeln!(file, "{padding}\",2024-02-29\r")?;
    }
    file.flush()?;
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()?;
    ensure!(
        output.status.success(),
        "shasum failed for {}",
        path.display()
    );
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .context("Missing SHA256")?
        .to_owned())
}

#[derive(Clone, Copy, Default)]
struct ProcessStats {
    rss: u64,
    disk_read: u64,
    disk_written: u64,
}

#[cfg(target_os = "macos")]
fn process_stats() -> Option<ProcessStats> {
    use std::ffi::c_void;

    // RUSAGE_INFO_V2 uses one UUID plus the 18 fixed u64 fields defined by
    // <sys/resource.h>. V2 adds the last two cumulative disk byte counters.
    #[repr(C)]
    #[derive(Default)]
    struct RusageV2 {
        uuid: [u8; 16],
        values: [u64; 18],
    }
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, output: *mut *mut c_void) -> i32;
    }
    let mut usage = RusageV2::default();
    // SAFETY: This is writable, aligned storage for the complete V2 ABI.
    // libproc's rusage_info_t address is an opaque pointer to that storage.
    let status = unsafe {
        proc_pid_rusage(
            i32::try_from(std::process::id()).ok()?,
            2,
            (&mut usage as *mut RusageV2).cast::<*mut c_void>(),
        )
    };
    (status == 0).then_some(ProcessStats {
        rss: usage.values[6],
        disk_read: usage.values[16],
        disk_written: usage.values[17],
    })
}

#[cfg(not(target_os = "macos"))]
fn process_stats() -> Option<ProcessStats> {
    None
}

fn timed<T>(label: &str, operation: impl FnOnce() -> T) -> (T, Value) {
    let before = process_stats();
    let (output, elapsed, after, sampled_peak) = thread::scope(|scope| {
        let (stop, receive) = mpsc::channel::<()>();
        let sampler = scope.spawn(move || {
            let mut highest = None;
            while let Err(mpsc::RecvTimeoutError::Timeout) = receive.recv_timeout(SAMPLE_INTERVAL) {
                highest = highest.max(process_stats().map(|stats| stats.rss));
            }
            highest
        });
        let started = Instant::now();
        let output = operation();
        let elapsed = started.elapsed();
        let after = process_stats();
        drop(stop);
        (output, elapsed, after, sampler.join().unwrap())
    });
    let peak = sampled_peak
        .max(before.map(|stats| stats.rss))
        .max(after.map(|stats| stats.rss));
    let disk_bytes = before.zip(after).map(|(before, after)| {
        json!({
            "read": after.disk_read.saturating_sub(before.disk_read),
            "written": after.disk_written.saturating_sub(before.disk_written),
        })
    });
    (
        output,
        json!({
            "case":label,
            "milliseconds":elapsed.as_secs_f64() * 1000.,
            "process_start_rss_bytes":before.map(|stats| stats.rss),
            "sampled_process_peak_rss_bytes":peak,
            "sampled_rss_growth_bytes":peak.zip(before).map(|(peak, before)| peak.saturating_sub(before.rss)),
            "macos_process_diskio_bytes":disk_bytes,
        }),
    )
}

fn page_request(offset: usize, limit: usize) -> QueryRequest {
    QueryRequest {
        offset,
        limit,
        ..Default::default()
    }
}

fn validate_page(
    page: &QueryResult,
    offset: usize,
    limit: usize,
    rows: usize,
    payload_len: usize,
) -> Result<()> {
    let expected_rows = rows.saturating_sub(offset).min(limit);
    ensure!(
        page.row_count() == expected_rows,
        "Wrong page size at {offset}"
    );
    ensure!(
        !page.total_rows_known || page.total_rows == rows,
        "Reported source count differs from the fixture at {offset}"
    );
    let padding = "x".repeat(payload_len);
    let long = format!("中文,\"quoted\"\r\n{padding}");
    for at in 0..expected_rows {
        let row = offset + at;
        ensure!(
            page.source_row_number(at) == Some(row + 1),
            "Source row number changed for logical record {row}"
        );
        ensure!(
            page.cell(at, 0).text == row.to_string(),
            "ID differs at {row}"
        );
        ensure!(
            page.cell(at, 1).text == format!("0{:08}", row % CODE_CATEGORIES),
            "Leading-zero code differs at {row}"
        );
        if row % 3 == 2 {
            ensure!(page.is_null(at, 2), "Empty flag lost its NULL at {row}");
        } else {
            ensure!(
                page.cell(at, 2).text.eq_ignore_ascii_case(if row % 3 == 0 {
                    "true"
                } else {
                    "false"
                }),
                "Typed flag differs at {row}"
            );
        }
        let expected = if multiline(row) { &long } else { &padding };
        ensure!(
            page.cell(at, 3).text == *expected,
            "Quoted payload differs at {row}"
        );
        ensure!(
            page.cell(at, 4).text == "2024-02-29",
            "Date differs at {row}"
        );
    }
    Ok(())
}

fn page(
    engine: &mut Engine,
    label: &str,
    offset: usize,
    rows: usize,
    payload_len: usize,
) -> Result<Value> {
    let request = page_request(offset, WINDOW_ROWS);
    let (result, mut report) = timed(label, || {
        engine.query_cancellable(&request, &AtomicBool::new(false))
    });
    let result = result?;
    validate_page(&result, offset, WINDOW_ROWS, rows, payload_len)?;
    report["offset"] = offset.into();
    report["requested_rows"] = WINDOW_ROWS.into();
    report["result_rows"] = result.row_count().into();
    report["result_arrow_bytes"] = result.data.memory_size().into();
    report["source_count_known"] = result.total_rows_known.into();
    engine.retain_snapshot(&result.snapshot_id)?;
    Ok(report)
}

fn validate_all_ids(result: &QueryResult, rows: usize) -> Result<()> {
    ensure!(result.total_rows_known && result.total_rows == rows);
    ensure!(result.loaded_rows == rows && result.row_count() == rows);
    ensure!(
        result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .eq(["id", "flag", "code"]),
        "Unexpected columns in full import"
    );
    let mut seen = 0;
    for batch in result.data.raw_batches() {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .context("Expected an inferred Int64 id column")?;
        ensure!(ids.null_count() == 0, "Unexpected missing IDs");
        for id in ids.values() {
            ensure!(*id == seen as i64, "ID ordering differs at record {seen}");
            seen += 1;
        }
    }
    ensure!(
        seen == rows,
        "Arrow batches returned {seen} IDs, expected {rows}"
    );
    for row in [0, rows / 2, rows - 1] {
        ensure!(result.source_row_number(row) == Some(row + 1));
    }
    Ok(())
}

fn cancel_after_index_started(engine: &mut Engine, rows: usize) -> Result<Value> {
    let request = QueryRequest {
        limit: 1,
        selected_columns: vec!["id".into()],
        load_to_end: true,
        ..Default::default()
    };
    let cancel = AtomicBool::new(false);
    let progress = QueryProgress::default();
    let (stop, receive) = mpsc::channel::<()>();
    let started = Instant::now();
    let (result, observed) = thread::scope(|scope| {
        let cancel_in_worker = &cancel;
        let monitor_progress = &progress;
        let monitor = scope.spawn(move || {
            loop {
                match receive.recv_timeout(Duration::from_millis(1)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let snapshot = monitor_progress.snapshot();
                        if snapshot.phase != QueryPhase::Complete
                            && (snapshot.processed_rows > 0
                                || snapshot.fraction.is_some_and(|f| f > 0.0 && f < 1.0))
                        {
                            cancel_in_worker.store(true, Ordering::Release);
                            return Some(snapshot);
                        }
                    }
                }
            }
        });
        let result = engine.query_cancellable_with_progress(&request, &cancel, &progress);
        drop(stop);
        (result, monitor.join().unwrap())
    });
    let elapsed = started.elapsed().as_secs_f64() * 1000.;
    if let Some(snapshot) = observed {
        let error = result.expect_err("Observed cancellation must discard the pending result");
        ensure!(
            format!("{error:#}").to_ascii_lowercase().contains("cancel"),
            "Expected cancellation, got {error:#}"
        );
        Ok(json!({
            "observed": true,
            "milliseconds": elapsed,
            "progress_phase": format!("{:?}", snapshot.phase),
            "progress_rows": snapshot.processed_rows,
            "progress_fraction": snapshot.fraction,
        }))
    } else {
        let result = result?;
        ensure!(result.row_count() == rows);
        Ok(json!({"observed": false, "reason": "Import finished before progress could be sampled"}))
    }
}

fn cancellation(path: &Path, rows: usize) -> Result<Value> {
    let mut engine = Engine::open_cancellable(&[path.to_owned()], &AtomicBool::new(false))?;
    let request = page_request(0, 3);
    let published = engine.query_cancellable(&request, &AtomicBool::new(false))?;
    let cancelled = AtomicBool::new(true);
    ensure!(
        engine
            .query_cancellable(&page_request(1, 3), &cancelled)
            .is_err(),
        "Pre-cancelled CSV query returned a result"
    );
    let during_work = cancel_after_index_started(&mut engine, rows)?;
    ensure!(published.row_count() == 3 && published.source_row_number(2) == Some(3));
    // Export through the retained snapshot. This exercises the normal path
    // after a cancelled operation, without modifying either source or engine.
    let retained = path.with_file_name("retained.csv");
    engine.export_result(&published.snapshot_id, &retained, ExportFormat::Csv)?;
    ensure!(
        engine
            .export_result(&published.snapshot_id, path, ExportFormat::Csv)
            .is_err(),
        "CSV import did not protect the source from export overwrite"
    );
    let mut reader = csv::Reader::from_path(retained)?;
    let ids = reader
        .records()
        .map(|record| {
            Ok(record?
                .get(0)
                .context("Missing ID in export")?
                .parse::<usize>()?)
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        ids == [0, 1, 2],
        "Prior CSV snapshot changed after cancellation"
    );
    Ok(
        json!({"pre_cancel_verified": true, "in_flight": during_work, "prior_snapshot_export_verified": true}),
    )
}

fn filter_and_sort(engine: &mut Engine) -> Result<Value> {
    let request = QueryRequest {
        offset: 25,
        limit: 40,
        selected_columns: vec!["id".into(), "flag".into()],
        filter: "\"flag\" = true".into(),
        sort: Some(("id".into(), true)),
        ..Default::default()
    };
    let (result, mut report) = timed("filter and sort a loaded range", || engine.query(&request));
    let result = result?;
    let expected = (request.offset..request.offset + request.limit)
        .rev()
        .filter(|row| row % 3 == 0)
        .collect::<Vec<_>>();
    ensure!(result.row_count() == expected.len());
    for (row, id) in expected.into_iter().enumerate() {
        ensure!(result.source_row_number(row) == Some(id + 1));
        ensure!(result.cell(row, 0).text == id.to_string());
    }
    report["result_rows"] = result.row_count().into();
    engine.retain_snapshot(&result.snapshot_id)?;
    Ok(report)
}

fn whole_source_filter(path: &Path, rows: usize, payload_len: usize) -> Result<Value> {
    let cancel = AtomicBool::new(false);
    let mut engine = Engine::open_cancellable(&[path.to_owned()], &cancel)?;
    let request = QueryRequest {
        selected_columns: vec!["id".into(), "flag".into(), "payload".into()],
        filter: format!("\"id\" % {MATCH_STRIDE} = 0"),
        whole_source: true,
        ..Default::default()
    };
    let progress = QueryProgress::default();
    let (result, mut report) = timed("whole-file sparse filter with wide text", || {
        engine.query_cancellable_with_progress(&request, &cancel, &progress)
    });
    let result = result?;
    let matches = rows.div_ceil(MATCH_STRIDE);
    ensure!(result.total_rows_known && result.total_rows == rows);
    ensure!(
        result.row_count() == matches && result.filtered_rows == Some(matches),
        "Unexpected result size for the sparse filter"
    );
    let padding = "x".repeat(payload_len);
    let long = format!("中文,\"quoted\"\r\n{padding}");
    let mut payload_bytes = 0usize;
    for (at, row) in (0..rows).step_by(MATCH_STRIDE).enumerate() {
        ensure!(result.source_row_number(at) == Some(row + 1));
        ensure!(result.cell(at, 0).text == row.to_string());
        match row % 3 {
            0 => ensure!(result.cell(at, 1).text == "true"),
            1 => ensure!(result.cell(at, 1).text == "false"),
            _ => ensure!(result.is_null(at, 1)),
        }
        let expected = if multiline(row) { &long } else { &padding };
        ensure!(
            result.cell(at, 2).text == *expected,
            "Selected text differs at source record {row}"
        );
        payload_bytes += expected.len();
    }
    report["source_rows"] = rows.into();
    report["result_rows"] = matches.into();
    report["result_arrow_bytes"] = result.data.memory_size().into();
    report["matched_text_bytes"] = payload_bytes.into();
    report["match_fraction"] = (matches as f64 / rows as f64).into();
    report["progress_fraction"] = progress.snapshot().fraction.into();
    Ok(report)
}

fn grouped_summary(
    engine: &mut Engine,
    snapshot: &str,
    rows: usize,
    cancel: &AtomicBool,
) -> Result<Value> {
    let request = AggregateRequest {
        group_by: vec!["flag".into()],
        metrics: vec![
            AggregateMetric {
                field: None,
                op: AggregateOp::CountRows,
            },
            AggregateMetric {
                field: Some("id".into()),
                op: AggregateOp::Sum,
            },
            AggregateMetric {
                field: Some("code".into()),
                op: AggregateOp::NUnique,
            },
        ],
        ..Default::default()
    };
    let progress = QueryProgress::default();
    let (result, mut report) = timed("grouped count, sum, and text nunique", || {
        engine.aggregate_snapshot_with_progress(snapshot, &request, cancel, &progress)
    });
    let result = result?;
    ensure!(result.input_rows == rows && result.data.row_count() == 3);
    for (at, (remainder, flag)) in [(1, "false"), (0, "true"), (2, "NULL")]
        .into_iter()
        .enumerate()
    {
        let group_rows = (rows - remainder).div_ceil(3);
        let last = remainder + (group_rows - 1) * 3;
        let sum = (remainder as u64 + last as u64) * group_rows as u64 / 2;
        // Stride 3 is coprime with the 1,024 codes, so each group visits all
        // codes in a cycle without repeats.
        ensure!(result.data.cell(at, 0).text == flag);
        ensure!(result.data.cell(at, 1).text == group_rows.to_string());
        ensure!(result.data.cell(at, 2).text == sum.to_string());
        ensure!(
            result.data.cell(at, 3).text == group_rows.min(CODE_CATEGORIES).to_string(),
            "Unexpected distinct code count for {flag}"
        );
    }
    report["input_rows"] = result.input_rows.into();
    report["result_rows"] = result.data.row_count().into();
    report["result_arrow_bytes"] = result.data.memory_size().into();
    let terminal = progress.snapshot();
    ensure!(
        terminal.phase == QueryPhase::Complete
            && terminal.fraction == Some(1.0)
            && terminal.processed_rows == rows
            && terminal.total_rows == Some(rows),
        "Aggregation progress did not complete: {terminal:?}"
    );
    report["progress_processed_rows"] = terminal.processed_rows.into();
    report["progress_fraction"] = terminal.fraction.into();
    Ok(report)
}

fn preview_reuse(path: &Path, rows: usize, cancel: &AtomicBool) -> Result<Value> {
    let options = CsvOptions {
        sample_records: 100_000,
        ..Default::default()
    };
    let (preview, prepare) = timed("preview first 100,000 records", || {
        CsvProbe::read(path, Some(options), cancel)
    });
    let preview = preview?;
    let sampled = rows.min(100_000);
    ensure!(preview.sampled_records == sampled);
    let (repeated, repeated_report) = timed("open after preview, repeat inference", || {
        Engine::open_csv(path, Some(preview.options.clone()), cancel)
    });
    let repeated = repeated?;
    let prepared = preview.prepared();
    let (reused, reused_report) = timed("open after preview, reuse inference", || {
        Engine::open_prepared_csv(path, prepared, cancel)
    });
    let reused = reused?;
    ensure!(
        repeated.info.columns == reused.info.columns
            && repeated.info.row_count == sampled
            && reused.info.row_count == sampled
            && repeated.info.row_count_known == reused.info.row_count_known,
        "Prepared preview changed the import schema or records"
    );
    Ok(json!({
        "sampled_records": sampled,
        "preview": prepare,
        "repeat": repeated_report,
        "reuse": reused_report,
    }))
}

fn type_mismatch(path: &Path) -> Result<Value> {
    let path = path.with_file_name("late-type-mismatch.csv");
    let mut source = BufWriter::new(File::create(&path)?);
    writeln!(source, "id,score")?;
    for id in 0..2000 {
        writeln!(source, "{id},12")?;
    }
    writeln!(source, "2000,late-text")?;
    source.flush()?;
    drop(source);

    let cancel = AtomicBool::new(false);
    let mut inferred = Engine::open_cancellable(std::slice::from_ref(&path), &cancel)?;
    let initial = inferred.query(&page_request(0, 3))?;
    let mismatch = inferred
        .query(&page_request(2000, 1))
        .expect_err("A later text value cannot be loaded into an inferred integer");
    let error = format!("{mismatch:#}");
    ensure!(
        error.contains("late-text") && error.contains("2001"),
        "Type mismatch diagnostic did not identify the source record: {error}"
    );
    let copied = path.with_file_name("prior-valid-page.csv");
    inferred.export_result(&initial.snapshot_id, &copied, ExportFormat::Csv)?;
    let options = CsvOptions {
        column_types: [(1, CsvColumnType::Text)].into(),
        ..Default::default()
    };
    let mut reopened = Engine::open_csv(&path, Some(options), &cancel)?;
    let late = reopened.query(&page_request(2000, 1))?;
    ensure!(late.source_row_number(0) == Some(2001));
    ensure!(late.cell(0, 1).text == "late-text");
    Ok(json!({
        "diagnostic": error,
        "prior_snapshot_export_verified": true,
        "text_override_verified": true,
    }))
}

fn check_sparse_browsing(gib: usize) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    ensure!(
        (1..=256).contains(&gib),
        "Choose a logical size from 1 to 256 GiB."
    );
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("browse.csv");
    let mut file = File::create(&path)?;
    file.write_all(b"id,payload\n")?;
    // Short first rows exercise a normal first screen. Later quoted UTF-8
    // fields contain sparse NUL bytes and embedded newlines. The reader must
    // still consume every logical byte and identify every record boundary.
    const HEAD: usize = 1000;
    for id in 0..HEAD {
        writeln!(file, "{id},first")?;
    }
    // Size the sparse extent first. Each marker lands at a fixed interval,
    // avoiding speculative allocation from a long sequence of appends.
    const GAP_MIB: usize = 4;
    let head_end = file.stream_position()?;
    let length = head_end + (gib as u64) * 1024 * 1024 * 1024;
    file.set_len(length)?;
    let rows = HEAD + gib * (1024 / GAP_MIB);
    for id in HEAD..rows {
        write!(file, "{id},\"line\n")?;
        let end = head_end + ((id - HEAD + 1) * GAP_MIB * 1024 * 1024) as u64;
        file.seek(SeekFrom::Start(end - 2))?;
        file.write_all(b"\"\n")?;
        if id == HEAD + 31 {
            ensure!(
                file.metadata()?.blocks() * 512 < 8 * 1024 * 1024,
                "This check needs a filesystem that supports sparse files."
            );
        }
    }
    let meta = file.metadata()?;
    drop(file);
    let cancel = AtomicBool::new(false);
    let (opened, open) = timed("open through first typed page", || -> Result<_> {
        let probe = CsvProbe::read_for_browse(&path, &cancel)?;
        let mut engine = Engine::open_prepared_csv(&path, probe.prepared(), &cancel)?;
        let (index, _, first) = engine.open_csv_view(&cancel)?;
        ensure!(first.row_count() == HEAD && first.source_row_number(HEAD - 1) == Some(HEAD));
        Ok((index, engine.resource_client()))
    });
    let (index, resources) = opened?;
    let (checked, scan) = timed(
        "index to EOF and fetch distant pages",
        || -> Result<usize> {
            let mut reads = 0;
            let mut last = 0;
            loop {
                let progress = index.progress();
                ensure!(progress.error.is_none(), "{:?}", progress.error);
                if progress.indexed_rows >= last + (1024 / GAP_MIB) || progress.complete {
                    let row = progress.indexed_rows - 1;
                    let page = index.read_page(row, 32, 1024 * 1024, &resources, &cancel)?;
                    ensure!(
                        page.source_row_number(0) == Some(row + 1)
                            && page.display_text(0, 0) == row.to_string()
                    );
                    last = progress.indexed_rows;
                    reads += 1;
                }
                if progress.complete {
                    ensure!(progress.indexed_rows == rows && progress.indexed_bytes == meta.len());
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
            Ok(reads)
        },
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "logical_file_bytes": meta.len(), "allocated_file_bytes": meta.blocks() * 512,
            "logical_records": rows, "checked_pages": checked?, "first_page": open, "background_index": scan,
        }))?
    );
    Ok(())
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let large = match arguments.next() {
        None => false,
        Some(flag) if flag == "--large" => true,
        Some(flag) if flag == "--browse-gib" => {
            let size = arguments.next().context("Missing size in GiB")?;
            ensure!(arguments.next().is_none(), "Expected one size in GiB");
            return check_sparse_browsing(size.to_str().context("Invalid size")?.parse()?);
        }
        _ => anyhow::bail!("Usage: check_csv_import [--large | --browse-gib N]"),
    };
    ensure!(
        arguments.next().is_none(),
        "Usage: check_csv_import [--large]"
    );
    let (rows, payload_len) = if large {
        (1_325_000, 4096)
    } else {
        (80_000, 320)
    };
    let folder = tempfile::Builder::new()
        .prefix("check-csv-import-")
        .tempdir()?;
    let path = folder.path().join("source.csv");
    let fixture_started = Instant::now();
    fixture(&path, rows, payload_len)?;
    let fixture_ms = fixture_started.elapsed().as_secs_f64() * 1000.;
    let before = fs::metadata(&path)?;
    ensure!(!large || before.len() > 5_000_000_000);
    let digest = sha256(&path)?;
    let cancel = AtomicBool::new(false);
    let (engine, mut open) = timed("open", || {
        Engine::open_cancellable(&[path.clone()], &cancel)
    });
    let mut engine = engine?;
    open["initial_source_rows_known"] = engine.info.row_count_known.into();
    let first = page(&mut engine, "first window", 0, rows, payload_len)?;
    let late_offset = rows - WINDOW_ROWS / 2;
    let late = page(
        &mut engine,
        "first late window, checkpoints initially unbuilt",
        late_offset,
        rows,
        payload_len,
    )?;
    let middle = page(
        &mut engine,
        "indexed middle window",
        rows / 2,
        rows,
        payload_len,
    )?;
    let late_repeat = page(
        &mut engine,
        "indexed late window after a different window",
        late_offset,
        rows,
        payload_len,
    )?;
    let filtered = filter_and_sort(&mut engine)?;
    drop(engine);
    let whole_filtered = whole_source_filter(&path, rows, payload_len)?;

    // Reopen to measure an independent EOF scan rather than reusing the
    // checkpoints already created by the first late page.
    let mut full = Engine::open_cancellable(&[path.clone()], &cancel)?;
    let request = QueryRequest {
        limit: 1,
        selected_columns: vec!["id".into(), "flag".into(), "code".into()],
        load_to_end: true,
        ..Default::default()
    };
    let progress = QueryProgress::default();
    let (loaded, mut full_report) = timed("load to EOF, projected to id, flag, and code", || {
        full.query_cancellable_with_progress(&request, &cancel, &progress)
    });
    let loaded = loaded?;
    validate_all_ids(&loaded, rows)?;
    full_report["result_rows"] = loaded.row_count().into();
    full_report["result_arrow_bytes"] = loaded.data.memory_size().into();
    full_report["progress_fraction"] = progress.snapshot().fraction.into();
    let summary = grouped_summary(&mut full, &loaded.snapshot_id, rows, &cancel)?;
    drop(loaded);
    drop(full);
    let preview_reuse = preview_reuse(&path, rows, &cancel)?;
    let cancellation = cancellation(&path, rows)?;
    let type_mismatch = type_mismatch(&path)?;
    ensure!(
        !large || cancellation["in_flight"]["observed"] == true,
        "Expected the large fixture to exercise cancellation during import"
    );
    let after = fs::metadata(&path)?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && digest == sha256(&path)?,
        "CSV source was modified"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "fixture_csv_bytes": before.len(),
            "fixture_rows": rows,
            "fixture_generation_ms": fixture_ms,
            "fixture_sha256": digest,
            "source_unchanged": true,
            "validation": "All projected IDs, selected pages, sparse filter matches and payloads, and grouped counts, sums and distinct counts validated.",
            "sampling_interval_ms": SAMPLE_INTERVAL.as_millis(),
            "scope": "Per-operation timings exclude generation, hashing and result validation. OS disk counters are process-wide and can exclude page-cache hits. A fresh checkpoint index is not a cold OS page cache; the fixture was just written and hashed. Sampled RSS growth is relative to the start of each operation and can be affected by allocator reuse. The full load retains only three narrow fields.",
            "operations": [open, first, late, middle, late_repeat, filtered, whole_filtered, full_report, summary],
            "preview_reuse": preview_reuse,
            "cancellation": cancellation,
            "type_mismatch": type_mismatch,
        }))?
    );
    Ok(())
}
