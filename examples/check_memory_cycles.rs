//! Repeated full imports and loaded-data filters through the application engine.
//! Usage: check_memory_cycles CYCLES HOLD_MS SETTLE_MS ROWS FILTER MATCHES FILE...
//! Supply one file per concurrent window. Fixtures must have the same rows and
//! match at the same original positions. PV_STRESS_SORT adds a descending sort.
//! PV_STRESS_FIELDS (comma-delimited) adds a field projection before closing.
//! PV_STRESS_SECOND_FILTER and PV_STRESS_SECOND_MATCHES add another filter.
//! PV_STRESS_RELEASE_MALLOC=1 tests Darwin's allocator pressure relief.
//! Set TMPDIR and PV_STRESS_WORKSPACE to the same dedicated empty directory
//! to count all temporary files, including DataFusion's lazy spill directory.
#![allow(dead_code)]
#[path = "../src/aggregation.rs"]
mod aggregation;
#[path = "../src/allocator.rs"]
mod allocator;
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
use arrow::array::Array;
use engine::{Engine, QueryRequest, QueryResult};
use serde::Serialize;
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Weak, mpsc},
    thread,
    time::{Duration, Instant, SystemTime},
};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Serialize)]
struct Process {
    rss_bytes: u64,
    footprint_bytes: u64,
    disk_read_bytes: u64,
    disk_written_bytes: u64,
}

#[cfg(target_os = "macos")]
fn process() -> Option<Process> {
    use std::ffi::c_void;

    // RUSAGE_INFO_V2 from the installed macOS SDK's <sys/resource.h>.
    #[derive(Default)]
    #[repr(C)]
    struct Rusage {
        uuid: [u8; 16],
        counters_before_memory: [u64; 6],
        resident_size: u64,
        physical_footprint: u64,
        counters_before_io: [u64; 8],
        disk_read: u64,
        disk_written: u64,
    }
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, usage: *mut *mut c_void) -> i32;
    }
    let mut usage = Rusage::default();
    // SAFETY: usage is writable, aligned storage for the complete fixed V2
    // structure. The synchronous libproc call does not retain the pointer.
    let status = unsafe {
        proc_pid_rusage(
            i32::try_from(std::process::id()).ok()?,
            2,
            (&mut usage as *mut Rusage).cast(),
        )
    };
    (status == 0).then_some(Process {
        rss_bytes: usage.resident_size,
        footprint_bytes: usage.physical_footprint,
        disk_read_bytes: usage.disk_read,
        disk_written_bytes: usage.disk_written,
    })
}

#[cfg(not(target_os = "macos"))]
fn process() -> Option<Process> {
    None
}

#[cfg(target_os = "macos")]
fn release_malloc() -> usize {
    unsafe extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: malloc/malloc.h permits a null zone for all registered zones and
    // a goal of zero for best effort reclamation without an explicit target.
    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) }
}

#[cfg(not(target_os = "macos"))]
fn release_malloc() -> usize {
    0
}

#[derive(Clone, Copy, Default, Serialize)]
struct Workspace {
    directories: usize,
    logical_bytes: u64,
    allocated_bytes: u64,
}

impl Workspace {
    fn visit(&mut self, path: &Path) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                self.directories += 1;
                self.visit(&entry.path());
            } else if metadata.is_file() {
                self.logical_bytes += metadata.len();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    self.allocated_bytes += metadata.blocks() * 512;
                }
            }
        }
    }

    fn sample(root: &Path, prefix: Option<&str>) -> Self {
        let mut usage = Self::default();
        let Some(prefix) = prefix else {
            usage.visit(root);
            return usage;
        };
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(prefix) {
                    usage.directories += 1;
                    usage.visit(&entry.path());
                }
            }
        }
        usage
    }
}

#[derive(Clone, Copy, Serialize)]
struct Sample {
    process: Option<Process>,
    workspace: Workspace,
}

#[derive(Default, Serialize)]
struct Peaks {
    samples: usize,
    rss_bytes: Option<u64>,
    footprint_bytes: Option<u64>,
    workspace: Workspace,
}

impl Peaks {
    fn include(&mut self, sample: Sample) {
        self.samples += 1;
        self.rss_bytes = self.rss_bytes.max(sample.process.map(|p| p.rss_bytes));
        self.footprint_bytes = self
            .footprint_bytes
            .max(sample.process.map(|p| p.footprint_bytes));
        let peak = &mut self.workspace;
        peak.directories = peak.directories.max(sample.workspace.directories);
        peak.logical_bytes = peak.logical_bytes.max(sample.workspace.logical_bytes);
        peak.allocated_bytes = peak.allocated_bytes.max(sample.workspace.allocated_bytes);
    }
}

struct Benchmark {
    started: Instant,
    workspace_root: PathBuf,
    workspace_prefix: Option<String>,
}

impl Benchmark {
    fn sample(&self) -> Sample {
        Sample {
            process: process(),
            workspace: Workspace::sample(&self.workspace_root, self.workspace_prefix.as_deref()),
        }
    }

    fn measure(
        &self,
        cycle: usize,
        phase: &str,
        operation: impl FnOnce() -> Result<Vec<Window>>,
    ) -> Result<Vec<Window>> {
        let before = self.sample();
        let at = Instant::now();
        let (output, elapsed, after, peak) = thread::scope(|scope| {
            let (stop, receive) = mpsc::channel::<()>();
            let sampler = scope.spawn(move || {
                let mut peak = Peaks::default();
                peak.include(before);
                while let Err(mpsc::RecvTimeoutError::Timeout) =
                    receive.recv_timeout(SAMPLE_INTERVAL)
                {
                    peak.include(self.sample());
                }
                peak
            });
            let output = operation();
            let elapsed = at.elapsed();
            let after = self.sample();
            drop(stop);
            let mut peak = sampler.join().expect("memory sampler panicked");
            peak.include(after);
            (output, elapsed, after, peak)
        });
        let windows = output.with_context(|| format!("Cycle {cycle}, {phase}"))?;
        let arrow_bytes: usize = windows
            .iter()
            .map(|window| window.result.data.memory_size())
            .sum();
        println!(
            "{}",
            json!({
                "cycle": cycle,
                "phase": phase,
                "elapsed_ms": elapsed.as_millis(),
                "run_ms": self.started.elapsed().as_millis(),
                "open_windows": windows.len(),
                "arrow_bytes": arrow_bytes,
                "rows": windows.iter().map(|window| window.result.row_count()).collect::<Vec<_>>(),
                "before": before,
                "after": after,
                "peak": peak,
            })
        );
        Ok(windows)
    }
}

struct Window {
    engine: Engine,
    request: QueryRequest,
    result: QueryResult,
}

impl Window {
    fn open(path: PathBuf, rows: usize) -> Result<Self> {
        let mut engine = Engine::open(std::slice::from_ref(&path))
            .with_context(|| format!("Open {}", path.display()))?;
        let request = QueryRequest {
            limit: rows,
            load_to_end: engine::has_delimited_text_extension(&path),
            ..Default::default()
        };
        let result = engine.query(&request)?;
        ensure!(
            result.total_rows_known && result.total_rows == rows && result.row_count() == rows,
            "{} should contain {rows} rows, got {}",
            path.display(),
            result.row_count()
        );
        ensure!(result.source_row_number(0) == Some(1));
        ensure!(result.source_row_number(rows - 1) == Some(rows));
        Ok(Self {
            engine,
            request,
            result,
        })
    }

    fn query(mut self, changed: impl FnOnce(&mut QueryRequest), rows: usize) -> Result<Self> {
        changed(&mut self.request);
        let next = self.engine.query(&self.request)?;
        ensure!(
            next.row_count() == rows,
            "Expected {rows}, got {}",
            next.row_count()
        );
        ensure!(next.timings.window_cache_hit && next.timings.window_load_ms == 0.);
        self.engine.retain_snapshot(&next.snapshot_id)?;
        self.result = next;
        Ok(self)
    }

    fn filtered_anchors(&self) -> Result<Vec<usize>> {
        let rows = self.result.row_count();
        let anchors = [0, rows / 2, rows.saturating_sub(1)]
            .into_iter()
            .filter(|&index| index < rows)
            .map(|index| {
                self.result
                    .source_row_number(index)
                    .context("Filtered row lost its original position")
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(anchors.is_sorted());
        Ok(anchors)
    }

    fn ownership_probes(&self) -> impl Iterator<Item = Weak<dyn Array>> {
        // Sample one batch per window; no strong array or buffer reference
        // escapes, so the probes cannot extend the allocation's lifetime.
        self.result
            .data
            .raw_batches()
            .first()
            .into_iter()
            .flat_map(|batch| batch.columns().iter().map(Arc::downgrade))
    }
}

fn parallel<T: Send, U: Send>(work: Vec<T>, run: impl Fn(T) -> Result<U> + Sync) -> Result<Vec<U>> {
    if work.len() == 1 {
        return work.into_iter().map(run).collect();
    }
    thread::scope(|scope| {
        let run = &run;
        let tasks: Vec<_> = work
            .into_iter()
            .map(|input| scope.spawn(move || run(input)))
            .collect();
        tasks
            .into_iter()
            .map(|task| task.join().expect("window worker panicked"))
            .collect()
    })
}

fn stamp(path: &Path) -> Result<(u64, SystemTime)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.len(), metadata.modified()?))
}

fn validate_filtered(windows: &[Window], expected: &mut Option<Vec<usize>>) -> Result<()> {
    for window in windows {
        let anchors = window.filtered_anchors()?;
        ensure!(
            expected.get_or_insert_with(|| anchors.clone()) == &anchors,
            "Filtered original positions changed across windows or cycles"
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    allocator::configure();
    let mut args = std::env::args().skip(1);
    let mut number = |name| -> Result<usize> {
        Ok(args
            .next()
            .with_context(|| format!("Missing {name}"))?
            .parse()?)
    };
    let cycles = number("cycle count")?;
    let hold = Duration::from_millis(number("hold milliseconds")? as u64);
    let settle = Duration::from_millis(number("settle milliseconds")? as u64);
    let rows = number("source row count")?;
    let filter = args.next().context("Missing filter")?;
    let matches: usize = args.next().context("Missing filtered row count")?.parse()?;
    let paths = args.map(PathBuf::from).collect::<Vec<_>>();
    ensure!(!paths.is_empty() && cycles > 0 && rows > 0 && matches <= rows);
    ensure!(!filter.trim().is_empty(), "Filter must be nonempty");
    let secondary = std::env::var("PV_STRESS_SECOND_FILTER")
        .ok()
        .map(|filter| -> Result<_> {
            let matches: usize = std::env::var("PV_STRESS_SECOND_MATCHES")?.parse()?;
            ensure!(matches <= rows && !filter.trim().is_empty());
            Ok((filter, matches))
        })
        .transpose()?;
    let sort = std::env::var("PV_STRESS_SORT").ok();
    let release_malloc_after_close =
        std::env::var("PV_STRESS_RELEASE_MALLOC").as_deref() == Ok("1");
    let fields = std::env::var("PV_STRESS_FIELDS").ok().map(|names| {
        names
            .split(',')
            .map(|part| part.trim().to_string())
            .collect::<Vec<_>>()
    });
    let stamps = paths
        .iter()
        .map(|path| stamp(path))
        .collect::<Result<Vec<_>>>()?;
    let mut workspace_root = std::env::temp_dir();
    let mut workspace_prefix = Some(format!("parquet-viewer-engine-{}-", std::process::id()));
    if let Some(directory) = std::env::var_os("PV_STRESS_WORKSPACE") {
        let directory = fs::canonicalize(directory)?;
        ensure!(directory == fs::canonicalize(&workspace_root)?);
        ensure!(
            fs::read_dir(&directory)?.next().is_none(),
            "Use an empty TMPDIR"
        );
        workspace_root = directory;
        workspace_prefix = None;
    }
    let benchmark = Benchmark {
        started: Instant::now(),
        workspace_root,
        workspace_prefix,
    };
    let mut first_matches = None;
    let mut second_matches = None;
    println!(
        "{}",
        json!({
            "pid": std::process::id(),
            "cycles": cycles,
            "files": paths,
            "file_lengths": stamps.iter().map(|stamp| stamp.0).collect::<Vec<_>>(),
            "source_rows_each": rows,
            "filter": filter,
            "filtered_rows_each": matches,
            "second_filter": secondary.as_ref().map(|(filter, _)| filter),
            "second_filtered_rows_each": secondary.as_ref().map(|(_, matches)| matches),
            "sample_interval_ms": SAMPLE_INTERVAL.as_millis(),
            "workspace_root": benchmark.workspace_root,
            "workspace_prefix": benchmark.workspace_prefix,
            "sort": sort,
            "fields": fields,
            "release_malloc_after_close": release_malloc_after_close,
        })
    );
    benchmark.measure(0, "startup", || Ok(Vec::new()))?;
    for cycle in 1..=cycles {
        let windows = benchmark.measure(cycle, "opened", || {
            parallel(paths.clone(), |path| Window::open(path, rows))
        })?;
        let probes: Vec<_> = windows.iter().flat_map(Window::ownership_probes).collect();
        let mut windows = benchmark.measure(cycle, "filtered", || {
            parallel(windows, |window| {
                window.query(|request| request.filter.clone_from(&filter), matches)
            })
        })?;
        validate_filtered(&windows, &mut first_matches)?;
        let mut match_count = matches;
        if let Some((filter, matches)) = &secondary {
            match_count = *matches;
            windows = benchmark.measure(cycle, "second_filter", || {
                parallel(windows, |window| {
                    window.query(|request| request.filter.clone_from(filter), match_count)
                })
            })?;
            validate_filtered(&windows, &mut second_matches)?;
        }
        if let Some(sort) = &sort {
            windows = benchmark.measure(cycle, "sorted", || {
                parallel(windows, |window| {
                    window.query(
                        |request| request.sort = Some((sort.clone(), true)),
                        match_count,
                    )
                })
            })?;
        }
        if let Some(fields) = &fields {
            windows = benchmark.measure(cycle, "projected", || {
                parallel(windows, |window| {
                    window.query(
                        |request| request.selected_columns.clone_from(fields),
                        match_count,
                    )
                })
            })?;
        }
        windows = benchmark.measure(cycle, "held", || {
            thread::sleep(hold);
            Ok(windows)
        })?;
        if windows.len() > 1 {
            windows = benchmark.measure(cycle, "closed_one_window", || {
                windows.remove(0);
                thread::sleep(hold);
                Ok(windows)
            })?;
        }
        benchmark.measure(cycle, "closed", || {
            drop(windows);
            ensure!(
                probes.iter().all(|array| array.strong_count() == 0),
                "A sampled Arrow array still has a strong owner after close"
            );
            Ok(Vec::new())
        })?;
        if release_malloc_after_close {
            let mut released_bytes = 0;
            benchmark.measure(cycle, "allocator_relief", || {
                released_bytes = release_malloc();
                Ok(Vec::new())
            })?;
            println!(
                "{}",
                json!({"cycle":cycle,"reported_allocator_released_bytes":released_bytes})
            );
        }
        benchmark.measure(cycle, "settled", || {
            thread::sleep(settle);
            Ok(Vec::new())
        })?;
    }
    for (path, before) in paths.iter().zip(stamps) {
        ensure!(stamp(path)? == before, "{} was modified", path.display());
    }
    println!(
        "{}",
        json!({"source_metadata_unchanged": true, "arrow_ownership_probes_released": true, "filtered_original_row_anchors": first_matches, "second_filter_original_row_anchors": second_matches})
    );
    Ok(())
}
