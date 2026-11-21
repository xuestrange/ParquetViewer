//! Reproducible Struct/List fixture for backend and sidebar GUI validation.
#![allow(dead_code)]
#[path = "../src/aggregation.rs"]
mod aggregation;
#[path = "../src/columnar.rs"]
mod columnar;
#[path = "../src/datafusion_session.rs"]
mod datafusion_session;
#[path = "../src/engine.rs"]
mod engine;
#[path = "../src/filtering.rs"]
mod filtering;
#[path = "../src/nested.rs"]
mod nested;
#[path = "../src/resource_budget.rs"]
mod resource_budget;
#[path = "../src/source_guard.rs"]
mod source_guard;

#[path = "../src/arrow_types.rs"]
mod arrow_types;
#[path = "../src/export.rs"]
mod export;
#[path = "../src/parquet_compat.rs"]
mod parquet_compat;
#[path = "../src/source_loader.rs"]
mod source_loader;
mod support;
#[cfg(test)]
#[path = "../src/test_support.rs"]
mod test_support;
use anyhow::{Result, ensure};
use filtering::{FilterCondition, FilterGroup, FilterOperator as Op, MatchMode};
use std::{path::PathBuf, sync::atomic::AtomicBool};

fn condition(field: &str, path: &[&str], operator: Op, value: &str) -> FilterCondition {
    FilterCondition {
        field: field.into(),
        path: path.iter().map(|part| (*part).into()).collect(),
        operator,
        value: value.into(),
        ..Default::default()
    }
}

fn main() -> Result<()> {
    let path = PathBuf::from("target/demo/nested-filters.parquet");
    std::fs::create_dir_all(path.parent().unwrap())?;
    if !path.exists() {
        support::make_nested(&path)?;
    }
    let mut engine =
        engine::Engine::open_cancellable(std::slice::from_ref(&path), &AtomicBool::new(false))?;
    ensure!(engine.info.row_count == 60, "Unexpected fixture");
    let cases = [
        (
            vec![condition("profile", &["age"], Op::Gt, "70")],
            vec![51, 52, 53, 54, 55, 56, 57, 58, 59],
        ),
        (
            vec![condition(
                "profile",
                &["stats", "score"],
                Op::Eq,
                "27.123456789012345678",
            )],
            vec![27],
        ),
        (
            vec![condition("profile", &["stats", "active"], Op::Eq, "true")],
            (0..60).filter(|i| i % 10 != 0 && i % 2 == 0).collect(),
        ),
        (
            vec![condition("profile", &["tags"], Op::Contains, "Alpha")],
            (0..60).filter(|i| i % 10 != 0 && i % 7 != 0).collect(),
        ),
        (
            vec![condition("profile", &["tags"], Op::Contains, "alpha")],
            vec![],
        ),
        (
            vec![condition("numbers", &[], Op::Contains, "28")],
            vec![27],
        ),
        (
            vec![condition("numbers", &[], Op::IsEmpty, "")],
            (0..60).filter(|i| i % 10 != 0 && i % 7 == 0).collect(),
        ),
        (
            vec![condition("numbers", &[], Op::IsNull, "")],
            (0..60).filter(|i| i % 10 == 0).collect(),
        ),
        (
            vec![condition("flags", &[], Op::Contains, "true")],
            (0..60).filter(|i| i % 2 == 0).collect(),
        ),
        (
            vec![condition("dates", &[], Op::Contains, "2024-02-29")],
            vec![28],
        ),
        (
            vec![
                condition("profile", &["stats", "active"], Op::Eq, "true"),
                condition("numbers", &[], Op::Contains, "33"),
            ],
            vec![32],
        ),
    ];
    let mut checks = Vec::new();
    for (conditions, expected) in cases {
        let group = FilterGroup {
            mode: MatchMode::All,
            conditions,
        };
        let expression = group.to_filter(&engine.info.columns)?;
        let result = engine.query(&engine::QueryRequest {
            limit: 60,
            filter: expression,
            ..Default::default()
        })?;
        let actual = (0..result.row_count())
            .map(|row| result.cell(row, 0).text.parse::<usize>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(actual == expected, "{group:?}: {actual:?} != {expected:?}");
        for (row, id) in actual.iter().enumerate() {
            ensure!(
                result.source_row_number(row) == Some(id + 1),
                "Original row identity changed"
            );
        }
        checks.push(serde_json::json!({"conditions":format!("{:?}",group.conditions),"rows":actual.len(),"ids":actual}));
        engine.discard_snapshot(&result.snapshot_id)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"fixture":path.canonicalize()?,"rows":60,"checks":checks,"scope":"Real Struct/List typed filters, exact Decimal, whole Text membership, Boolean, Date, empty/NULL, All conditions and original row identities. Also contains an embedded SVG Photo for sidebar GUI validation."})
        )?
    );
    Ok(())
}
