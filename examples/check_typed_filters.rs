//! Real typed filter fixture and query validation; does not change user sources.
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
mod support;
#[cfg(test)]
#[path = "../src/test_support.rs"]
mod test_support;
use anyhow::{Result, ensure};
use filtering::{FilterCondition, FilterGroup, FilterOperator as Op, MatchMode};
use std::{path::PathBuf, sync::atomic::AtomicBool};
fn main() -> Result<()> {
    let path = PathBuf::from("target/demo/typed-filters.parquet");
    std::fs::create_dir_all(path.parent().unwrap())?;
    if !path.exists() {
        support::make_typed(&path)?;
    }
    let mut engine =
        engine::Engine::open_cancellable(std::slice::from_ref(&path), &AtomicBool::new(false))?;
    ensure!(engine.info.row_count == 60, "Unexpected fixture");
    let mut checks = Vec::new();
    for (field, operator, value, upper, expected) in [
        (
            "amount",
            Op::Gt,
            "10.123456789012345678",
            "",
            (11..60).collect::<Vec<_>>(),
        ),
        (
            "amount",
            Op::Between,
            "27.123456789012345678",
            "29.123456789012345678",
            vec![27, 28, 29],
        ),
        (
            "event_date",
            Op::Between,
            "2024-02-28",
            "2024-03-01",
            vec![27, 28, 29],
        ),
        ("event_date", Op::Gt, "2024-02-28", "", (28..60).collect()),
        (
            "happened_at",
            Op::Gt,
            "2024-02-28 12:34:56.123456789",
            "",
            (28..60).collect(),
        ),
        (
            "local_time",
            Op::Between,
            "12:34:10",
            "12:34:12",
            vec![10, 11, 12],
        ),
        (
            "active",
            Op::Eq,
            "true",
            "",
            (0..60).filter(|i| i % 2 == 0).collect(),
        ),
        (
            "title",
            Op::Contains,
            "item 2",
            "",
            (0..60)
                .filter(|i| i % 7 != 0 && format!("Item {i}").contains("Item 2"))
                .collect(),
        ),
        ("items", Op::IsNotNull, "", "", (0..60).collect()),
        (
            "numeric_text",
            Op::Contains,
            "2",
            "",
            (0..60).filter(|i| i.to_string().contains('2')).collect(),
        ),
    ] {
        let expression = FilterGroup {
            mode: MatchMode::All,
            conditions: vec![FilterCondition {
                field: field.into(),
                path: Vec::new(),
                operator,
                value: value.into(),
                second_value: upper.into(),
            }],
        }
        .to_filter(&engine.info.columns)?;
        let result = engine.query(&engine::QueryRequest {
            limit: 60,
            filter: expression,
            ..Default::default()
        })?;
        let actual = (0..result.row_count())
            .map(|r| result.cell(r, 0).text.parse::<usize>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            actual == expected,
            "{field}/{operator:?}: {actual:?} != {expected:?}"
        );
        for (row, id) in actual.iter().enumerate() {
            ensure!(
                result.source_row_number(row) == Some(id + 1),
                "Row provenance changed"
            );
        }
        checks.push(serde_json::json!({"field":field,"kind":format!("{:?}",filtering::field_kind(&result.columns.iter().find(|c|c.name==field).unwrap().data_type)),"operator":format!("{operator:?}"),"rows":actual.len(),"ids":actual}));
        engine.discard_snapshot(&result.snapshot_id)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"fixture":path.canonicalize()?,"rows":60,"checks":checks,"scope":"Actual typed DataFusion filtering, leap-day/date/time/nanoseconds/decimal precision, Boolean/text/NULL and original row identities"})
        )?
    );
    Ok(())
}
