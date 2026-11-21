//! Real large-Parquet filtering and source-row provenance validation.
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
use anyhow::{Result, ensure};
use std::{path::PathBuf, sync::atomic::AtomicBool, time::Instant};
fn main() -> Result<()> {
    let path = PathBuf::from("target/large-data/struct-5gb.parquet");
    let mut engine =
        engine::Engine::open_cancellable(std::slice::from_ref(&path), &AtomicBool::new(false))?;
    let total = engine.info.row_count;
    let mut runs = Vec::new();
    for (label, offset, limit, filter, expected, sort) in [
        (
            "numeric_all",
            0,
            total,
            "id >= 1300000 AND id < 1301000",
            1000,
            None,
        ),
        (
            "numeric_all_repeat",
            0,
            total,
            "id >= 1290000 AND id < 1290010",
            10,
            None,
        ),
        ("text_all", 0, total, "note = 'record 2200001'", 1, None),
        (
            "numeric_window_sort",
            1000000,
            500000,
            "id >= 1499950 AND id < 1500000",
            50,
            Some(("id".to_owned(), true)),
        ),
    ] {
        let start = Instant::now();
        let result = engine.query(&engine::QueryRequest {
            offset,
            limit,
            filter: filter.into(),
            sort,
            ..Default::default()
        })?;
        ensure!(result.row_count() == expected, "{label} wrong count");
        let ids =
            [0, result.row_count() / 2, result.row_count() - 1].map(|row| result.cell(row, 0).text);
        let mut source_row_numbers = Vec::new();
        for row in [0, result.row_count() / 2, result.row_count() - 1] {
            let id = result.cell(row, 0).text.parse::<usize>()?;
            let number = result
                .source_row_number(row)
                .ok_or_else(|| anyhow::anyhow!("Missing original row number"))?;
            ensure!(number == id + 1, "{label} original row number mismatch");
            ensure!(
                result.cell(row, 1).json.is_some(),
                "Missing typed nested value"
            );
            source_row_numbers.push(number);
        }
        runs.push(serde_json::json!({"case":label,"ms":start.elapsed().as_millis(),"rows":result.row_count(),"arrow_bytes":result.data.memory_size(),"ids":ids,"source_row_numbers":source_row_numbers}));
        engine.discard_snapshot(&result.snapshot_id)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"source":path.canonicalize()?,"source_bytes":std::fs::metadata(path)?.len(),"source_rows":total,"runs":runs,"scope":"single local run; warm OS cache may differ; not GUI/FPS/peak RSS"})
        )?
    );
    Ok(())
}
