//! Same-fixture loaded-window filtering/sorting performance and correctness.
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
use engine::{Engine, QueryRequest, QueryResult};
use filtering::{FilterCondition, FilterGroup, FilterOperator as Op};
use std::{path::PathBuf, sync::atomic::AtomicBool, time::Instant};
fn c(field: &str, op: Op, value: String) -> FilterCondition {
    FilterCondition {
        field: field.into(),
        operator: op,
        value,
        ..Default::default()
    }
}
fn main() -> Result<()> {
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .context("Usage: check_migration SOURCE.parquet")?,
    );
    let original = std::fs::metadata(&path)?;
    let opening = Instant::now();
    let mut engine =
        Engine::open_cancellable(std::slice::from_ref(&path), &AtomicBool::new(false))?;
    let open_ms = opening.elapsed().as_secs_f64() * 1000.;
    let total = engine.info.row_count;
    ensure!(
        total >= 2000,
        "Use a sequential id/details/note fixture with at least 2000 rows"
    );
    for name in ["id", "details", "note"] {
        ensure!(
            engine.info.columns.iter().any(|c| c.name == name),
            "Missing fixture field {name}"
        );
    }
    let middle = total / 2;
    let mut exact = total - 3;
    while exact % 11 == 0 {
        exact -= 1;
    }
    let mut nested = c("details", Op::Eq, "上海".into());
    nested.path = vec!["customer".into(), "city".into()];
    let cases = vec![
        ("load_all", vec![], None, total, 0, total - 1),
        (
            "numeric_filter",
            vec![
                c("id", Op::Ge, middle.to_string()),
                c("id", Op::Lt, (middle + 1000).to_string()),
            ],
            None,
            1000,
            middle,
            middle + 999,
        ),
        (
            "change_numeric_filter",
            vec![
                c("id", Op::Ge, (middle - 1000).to_string()),
                c("id", Op::Lt, (middle - 990).to_string()),
            ],
            None,
            10,
            middle - 1000,
            middle - 991,
        ),
        (
            "nested_struct_filter",
            vec![nested, c("id", Op::Ge, (total - 1000).to_string())],
            None,
            500,
            total - 1000,
            total - 2,
        ),
        (
            "text_filter",
            vec![c("note", Op::Eq, format!("record {exact}"))],
            None,
            1,
            exact,
            exact,
        ),
        (
            "sort_all_desc",
            vec![],
            Some(("id".into(), true)),
            total,
            total - 1,
            0,
        ),
        ("clear_filter_sort", vec![], None, total, 0, total - 1),
    ];
    let id = engine
        .info
        .columns
        .iter()
        .position(|c| c.name == "id")
        .unwrap();
    let mut previous: Option<QueryResult> = None;
    let mut runs = Vec::new();
    for (label, conditions, sort, count, first, last) in cases {
        let filter = FilterGroup {
            conditions,
            ..Default::default()
        }
        .to_filter(&engine.info.columns)?;
        let request = QueryRequest {
            limit: total,
            filter,
            sort,
            ..Default::default()
        };
        let start = Instant::now();
        let result = engine
            .query(&request)
            .with_context(|| format!("{label} failed"))?;
        let ms = start.elapsed().as_secs_f64() * 1000.;
        ensure!(
            result.row_count() == count,
            "{label}: expected {count} rows, got {}",
            result.row_count()
        );
        ensure!(
            result.loaded_rows == total,
            "{label}: changed imported window"
        );
        for (row, expected) in [(0, first), (count - 1, last)] {
            ensure!(
                result.cell(row, id).text == expected.to_string(),
                "{label}: wrong id anchor"
            );
            ensure!(
                result.source_row_number(row) == Some(expected + 1),
                "{label}: changed source row number"
            );
        }
        let details = engine
            .info
            .columns
            .iter()
            .position(|c| c.name == "details")
            .unwrap();
        ensure!(
            result.cell(0, details).json.is_some(),
            "{label}: missing nested typed value"
        );
        runs.push(serde_json::json!({"case":label,"milliseconds":ms,"rows":count,"first_id":first,"last_id":last,"retained_arrow_bytes":result.data.memory_size()}));
        if let Some(old) = previous.replace(result) {
            engine.discard_snapshot(&old.snapshot_id)?;
            drop(old);
        }
    }
    let m = std::fs::metadata(&path)?;
    ensure!(
        m.len() == original.len() && m.modified()? == original.modified()?,
        "Source changed"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"source":path.canonicalize()?,"source_bytes":m.len(),"source_rows":total,"open_ms":open_ms,"runs":runs,"source_size_mtime_unchanged":true,"scope":"Same sequential loaded-window interaction workload; last displayed snapshot retained until replacement succeeds. Warm OS cache; query wall times only, process metrics in companion sampler."})
        )?
    );
    Ok(())
}
