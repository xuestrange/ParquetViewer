//! Exercise the application's real Engine against a multi-gigabyte source.
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
use engine::{Engine, ExportFormat, QueryRequest};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Instant,
};

fn sha256(path: &Path) -> Result<String> {
    let output = std::process::Command::new("/usr/bin/shasum")
        .arg("-a")
        .arg("256")
        .arg(path)
        .output()?;
    ensure!(output.status.success(), "SHA-256 failed");
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .context("Missing hash")?
        .to_owned())
}

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/large-data/struct-5gb.parquet".into());
    let meta = std::fs::metadata(&path)?;
    ensure!(
        meta.len() >= 5_000_000_000,
        "This test requires a real >=5 GB file"
    );
    let before = sha256(&path)?;
    let started = Instant::now();
    let cancel = AtomicBool::new(false);
    let mut engine = Engine::open_cancellable(&[path.clone()], &cancel)?;
    let open_ms = started.elapsed().as_millis();
    ensure!(engine.info.row_count == 2_600_000, "Wrong source row count");
    let started = Instant::now();
    let first = engine.query(&QueryRequest::default())?;
    ensure!(
        first.row_count() == 1000,
        "Initial read was not limited to 1000 rows"
    );
    let first_details = first.cell(0, 1);
    let details = first_details
        .json
        .as_ref()
        .context("Missing typed struct projection")?;
    ensure!(
        details["customer"]["city"] == "上海"
            && details["measurements"]
                .as_array()
                .is_some_and(|a| a.len() == 2),
        "Nested content mismatch"
    );
    let first_window_ms = started.elapsed().as_millis();
    let started = Instant::now();
    let filtered = engine.query(&QueryRequest {
        filter: "id >= 500 AND id < 510".into(),
        sort: Some(("id".into(), true)),
        ..QueryRequest::default()
    })?;
    ensure!(
        filtered.row_count() == 10 && filtered.cell(0, 0).text == "509",
        "Typed filter/sort mismatch"
    );
    let filter_sort_ms = started.elapsed().as_millis();
    let exports = path.parent().unwrap().join("exports");
    std::fs::create_dir_all(&exports)?;
    for (format, ext) in [
        (ExportFormat::Json, "json"),
        (ExportFormat::Parquet, "parquet"),
        (ExportFormat::Xls, "xls"),
        (ExportFormat::Xlsx, "xlsx"),
    ] {
        engine.export_result_columns_cancellable(
            &filtered.snapshot_id,
            &[],
            &exports.join(format!("window.{ext}")),
            format,
            &cancel,
        )?;
        ensure!(
            engine
                .export_result_columns_cancellable(
                    &filtered.snapshot_id,
                    &[],
                    &path,
                    format,
                    &cancel
                )
                .is_err(),
            "Source overwrite was allowed for {ext}"
        );
    }
    let started = Instant::now();
    let end = engine.query(&QueryRequest {
        offset: 2_599_000,
        limit: 1000,
        selected_columns: vec!["id".into(), "details".into()],
        ..QueryRequest::default()
    })?;
    ensure!(
        end.row_count() == 1000 && end.cell(999, 0).text == "2599999",
        "Final source page mismatch"
    );
    let last_window_ms = started.elapsed().as_millis();
    ensure!(before == sha256(&path)?, "Source bytes changed");
    ensure!(
        meta.modified()? == std::fs::metadata(&path)?.modified()?,
        "Source modification time changed"
    );
    let report = serde_json::json!({
        "source":path.canonicalize()?, "file_bytes":meta.len(), "source_rows":engine.info.row_count,
        "open_ms":open_ms,"first_1000_with_struct_ms":first_window_ms,
        "loaded_window_filter_sort_ms":filter_sort_ms,"last_1000_with_struct_ms":last_window_ms,
        "all_five_exports":true,"all_five_source_overwrites_blocked":true,
        "source_sha256_before_and_after":before,"source_mtime_unchanged":true,
        "scope":"windowed access, typed nested values, filter/sort and derived exports; no full 5GB materialization"
    });
    let report_path = path.parent().unwrap().join("validation.json");
    source_guard::save_copy(
        &report_path,
        &[],
        serde_json::to_string_pretty(&report)?.as_bytes(),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
