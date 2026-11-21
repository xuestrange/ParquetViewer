//! Same-body baseline/current query-index memory and source-row regression probe.
//! --make [ROWS] prepares the owned fixture outside measured operations.
//! [SOURCE] runs loading, a broad filter, stable descending sort and clear.
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

use anyhow::{Context, Result, ensure};
use arrow::{
    array::{Array, UInt64Array},
    record_batch::RecordBatch,
};
use engine::{Engine, ExportFormat, QueryRequest, QueryResult};
use parquet::{
    arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder},
    basic::Compression,
    file::properties::WriterProperties,
};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_ROWS: usize = 10_000_000;
fn fixture() -> PathBuf {
    PathBuf::from("target/demo/index-memory.parquet")
}
fn make_fixture(path: &Path, rows: usize) -> Result<()> {
    ensure!(rows > 100_000, "Fixture needs over 100,000 rows");
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path.parent().context("Fixture parent missing")?)?;
    let schema = Arc::new(arrow::datatypes::Schema::new(vec![
        arrow::datatypes::Field::new("id", arrow::datatypes::DataType::UInt64, false),
    ]));
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .set_max_row_group_row_count(Some(262_144))
        .build();
    let mut writer = ArrowWriter::try_new(File::create(path)?, schema.clone(), Some(properties))?;
    for start in (0..rows).step_by(262_144) {
        let end = (start + 262_144).min(rows);
        let array = Arc::new(UInt64Array::from_iter_values(start as u64..end as u64));
        writer.write(&RecordBatch::try_new(schema.clone(), vec![array])?)?;
    }
    writer.close()?;
    Ok(())
}
fn stamp(path: &Path) -> Result<Value> {
    let m = fs::metadata(path)?;
    Ok(json!({"bytes":m.len(),"modified_ns":m.modified()?.duration_since(UNIX_EPOCH)?.as_nanos()}))
}

#[derive(Clone, Copy, Default)]
struct Memory {
    rss: u64,
    footprint: u64,
}
#[cfg(target_os = "macos")]
fn memory() -> Option<Memory> {
    #[repr(C)]
    #[derive(Default)]
    struct Rusage {
        uuid: [u8; 16],
        user: u64,
        system: u64,
        idle: u64,
        interrupt: u64,
        pageins: u64,
        wired: u64,
        rss: u64,
        footprint: u64,
        start: u64,
        end: u64,
    }
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut *mut std::ffi::c_void) -> i32;
    }
    let mut info = Rusage::default();
    // SAFETY: Apple's fixed RUSAGE_INFO_V0 ABI is 96 aligned bytes, queried for
    // this process only; libproc writes synchronously and retains no pointer.
    let status = unsafe {
        proc_pid_rusage(
            std::process::id() as i32,
            0,
            (&mut info as *mut Rusage).cast(),
        )
    };
    (status == 0).then_some(Memory {
        rss: info.rss,
        footprint: info.footprint,
    })
}
#[cfg(not(target_os = "macos"))]
fn memory() -> Option<Memory> {
    None
}
fn memory_json(value: Option<Memory>) -> Value {
    value.map_or(
        Value::Null,
        |v| json!({"rss_bytes":v.rss,"physical_footprint_bytes":v.footprint}),
    )
}
fn spill() -> (u64, u64) {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    fn visit(path: &Path, logical: &mut u64, allocated: &mut u64) {
        let Ok(items) = fs::read_dir(path) else {
            return;
        };
        for item in items.flatten() {
            let Ok(m) = item.metadata() else { continue };
            if m.is_dir() {
                visit(&item.path(), logical, allocated)
            } else if m.is_file() {
                *logical = logical.saturating_add(m.len());
                #[cfg(unix)]
                {
                    *allocated = allocated.saturating_add(m.blocks() * 512);
                }
            }
        }
    }
    let mut logical = 0;
    let mut allocated = 0;
    if let Ok(items) = fs::read_dir(std::env::temp_dir()) {
        let prefix = format!("parquet-viewer-engine-{}-", std::process::id());
        for item in items.flatten() {
            if item.file_name().to_string_lossy().starts_with(&prefix) {
                visit(&item.path(), &mut logical, &mut allocated)
            }
        }
    }
    (logical, allocated)
}
#[derive(Default)]
struct Peaks {
    rss: u64,
    footprint: u64,
    spill: u64,
    allocated_spill: u64,
    samples: usize,
}
struct Sampler {
    running: Arc<AtomicBool>,
    peaks: Arc<Mutex<Peaks>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Sampler {
    fn start() -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let peaks = Arc::new(Mutex::new(Peaks::default()));
        let r = running.clone();
        let p = peaks.clone();
        let worker = thread::spawn(move || {
            while r.load(Ordering::Acquire) {
                let current = memory();
                let (s, a) = spill();
                {
                    let mut p = p.lock().unwrap();
                    p.samples += 1;
                    if let Some(m) = current {
                        p.rss = p.rss.max(m.rss);
                        p.footprint = p.footprint.max(m.footprint)
                    }
                    p.spill = p.spill.max(s);
                    p.allocated_spill = p.allocated_spill.max(a);
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        Self {
            running,
            peaks,
            worker: Some(worker),
        }
    }
    fn finish(mut self) -> Value {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let p = self.peaks.lock().unwrap();
        json!({"sample_interval_ms":5,"samples":p.samples,"max_sampled_rss_bytes":p.rss,"max_sampled_physical_footprint_bytes":p.footprint,"max_sampled_query_spill_logical_bytes":p.spill,"max_sampled_query_spill_allocated_bytes":p.allocated_spill})
    }
}
impl Drop for Sampler {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn expected(label: &str, row: usize, total: usize) -> usize {
    if label == "descending" {
        total - 1 - row
    } else if label == "broad_filter" {
        row + 1
    } else {
        row
    }
}
fn anchors(result: &QueryResult, label: &str, total: usize) -> Result<Value> {
    let mut positions = vec![
        0,
        1,
        32767,
        32768,
        65535,
        65536,
        result.row_count() / 2,
        result.row_count() - 1,
    ];
    positions.retain(|&p| p < result.row_count());
    positions.sort_unstable();
    positions.dedup();
    let mut output = Vec::new();
    for row in positions {
        let id = expected(label, row, total);
        ensure!(
            result.cell(row, 0).text.parse::<usize>()? == id,
            "{label}: wrong id at {row}"
        );
        ensure!(
            result.source_row_number(row) == Some(id + 1),
            "{label}: wrong source row at {row}"
        );
        output.push(json!({"row":row,"id":id,"source_row":id+1}));
    }
    Ok(Value::Array(output))
}
fn publish(
    engine: &mut Engine,
    current: &mut Option<QueryResult>,
    next: QueryResult,
) -> Result<()> {
    let id = next.snapshot_id.clone();
    let old = current.replace(next);
    engine.retain_snapshot(&id)?;
    drop(old);
    Ok(())
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--make") {
        let path = fixture();
        let rows = args
            .get(1)
            .map(|arg| arg.parse::<usize>())
            .transpose()?
            .unwrap_or(DEFAULT_ROWS);
        make_fixture(&path, rows)?;
        println!(
            "{}",
            json!({"fixture":path,"rows":rows,"bytes":fs::metadata(&path)?.len()})
        );
        return Ok(());
    }
    let path = args.first().map_or_else(fixture, PathBuf::from);
    ensure!(
        path.exists(),
        "Prepare the fixture with --make first: {}",
        path.display()
    );
    let before = stamp(&path)?;
    let opened = Instant::now();
    let mut engine = Engine::open(std::slice::from_ref(&path))?;
    let open_ms = opened.elapsed().as_secs_f64() * 1000.;
    let total = engine.info.row_count;
    ensure!(total > 100_000, "Fixture too small");
    let mut current = None;
    let mut phases = Vec::new();
    for label in ["load", "broad_filter", "descending", "clear"] {
        let before_memory = memory_json(memory());
        let sampler = Sampler::start();
        let mut request = QueryRequest {
            selected_columns: vec!["id".into()],
            limit: total,
            ..Default::default()
        };
        if matches!(label, "broad_filter" | "descending") {
            request.filter = "id >= 1".into()
        }
        if label == "descending" {
            request.sort = Some(("id".into(), true))
        }
        let started = Instant::now();
        let result = engine.query(&request)?;
        let query_ms = started.elapsed().as_secs_f64() * 1000.;
        let count = result.row_count();
        ensure!(
            count
                == if matches!(label, "broad_filter" | "descending") {
                    total - 1
                } else {
                    total
                },
            "{label}:wrong count"
        );
        let timings = json!({"window_cache_hit":result.timings.window_cache_hit,"window_load_ms":result.timings.window_load_ms,"index_query_ms":result.timings.index_query_ms,"adapter_ms":result.timings.adapter_ms,"arrow_start_ms":result.timings.arrow_start_ms});
        let start = Instant::now();
        publish(&mut engine, &mut current, result)?;
        let publish_ms = start.elapsed().as_secs_f64() * 1000.;
        let after_memory = memory_json(memory());
        let peaks = sampler.finish();
        let result = current.as_ref().unwrap();
        let checked = anchors(result, label, total)?;
        phases.push(json!({"phase":label,"rows":count,"query_milliseconds":query_ms,"publish_and_old_result_drop_milliseconds":publish_ms,"timings":timings,"logical_arrow_bytes":result.data.memory_size(),"before_memory":before_memory,"after_publish_memory":after_memory,"sampled_peaks":peaks,"correctness_anchors":checked}));
    }
    // Verify snapshot export remains exact after replacement. This is outside all
    // query phase timings and uses a small bounded range to avoid unnecessary I/O.
    let export_request = QueryRequest {
        selected_columns: vec!["id".into()],
        offset: 12345,
        limit: 40000,
        filter: "id >= 20000".into(),
        sort: Some(("id".into(), true)),
        ..Default::default()
    };
    let result = engine.query(&export_request)?;
    ensure!(result.row_count() == 32345, "Export range wrong");
    let directory = PathBuf::from("target/index-memory-export");
    fs::create_dir_all(&directory)?;
    let output = directory.join(format!(
        "check-{}-{}.parquet",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    engine.export_result_cancellable(
        &result.snapshot_id,
        &output,
        ExportFormat::Parquet,
        &AtomicBool::new(false),
    )?;
    let batches = ParquetRecordBatchReaderBuilder::try_new(File::open(&output)?)?
        .build()?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut position = 0;
    for batch in batches {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .context("Export id type changed")?;
        for value in ids.values() {
            ensure!(*value == 52344 - position as u64, "Export order changed");
            position += 1;
        }
    }
    ensure!(position == 32345, "Exported row count wrong");
    let export_bytes = fs::metadata(&output)?.len();
    fs::remove_file(output)?;
    ensure!(before == stamp(&path)?, "Source size/mtime changed");
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"source":fs::canonicalize(path)?,"source_rows":total,"source_metadata":before,"open_milliseconds":open_ms,"phases":phases,"export_rows_checked":position,"export_bytes":export_bytes,"source_metadata_unchanged":true,"scope":"Same-body serial load/filter/sort/clear query phases; timed query excludes correctness cell formatting and export. Peak 5ms samples cover query and publishing/drop of old snapshot; per-phase OS before/after snapshots reflect allocator retention/compression, not live Arrow ownership. The source fixture is prepared separately; OS page cache is uncontrolled. Query spill samples count simultaneously present private workspace bytes, not cumulative disk writes."})
        )?
    );
    Ok(())
}
