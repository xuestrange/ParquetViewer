//! Create a typed 5,000-row by 1,000-column Parquet fixture.
mod support;
use anyhow::{Result, ensure};
use arrow::array::{Array, Decimal128Array, Int64Array, ListArray, StringArray};
use std::{path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo/wide-arrow.parquet".into());
    let started = Instant::now();
    support::make_wide(&destination)?;
    let bytes = std::fs::metadata(&destination)?.len();
    ensure!(bytes > 0, "Fixture was not written");
    let mut rows = 0usize;
    let mut columns = 0usize;
    let mut nulls = Vec::<usize>::new();
    for batch in support::read_all(&destination)? {
        let start = rows;
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        for local in 0..batch.num_rows() {
            let id = start + local;
            ensure!(
                ids.value(local) == id as i64,
                "Stored first-column row identity changed"
            );
            if [0, 2500, 4999].contains(&id) {
                for (column, position) in [(500, "MIDDLE"), (999, "LAST")] {
                    let array = batch
                        .column(column)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap();
                    ensure!(
                        array.value(local) == format!("{position} · row={id} · col_{column:04}"),
                        "Stored middle/last-column anchor changed"
                    );
                }
            }
            if id == 0 {
                ensure!(
                    batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<Decimal128Array>()
                        .unwrap()
                        .value(local)
                        == 9_007_199_254_740_993_123_456_789_012_340_001i128,
                    "Stored Decimal precision changed"
                );
                ensure!(
                    batch
                        .column(250)
                        .as_any()
                        .downcast_ref::<Decimal128Array>()
                        .unwrap()
                        .value(local)
                        == support::LARGE_INTEGER_BASE,
                    "Stored large integer changed"
                );
            }
            if id == 1 {
                let values = batch
                    .column(49)
                    .as_any()
                    .downcast_ref::<ListArray>()
                    .unwrap()
                    .value(local);
                let values = values.as_any().downcast_ref::<Int64Array>().unwrap();
                ensure!(
                    values.len() == 4
                        && values.value(0) == 1
                        && values.value(1) == 49
                        && values.is_null(2)
                        && values.value(3) == 2,
                    "Stored nested List changed"
                );
            }
        }
        rows += batch.num_rows();
        columns = batch.num_columns();
        if nulls.is_empty() {
            nulls.resize(columns, 0);
        }
        for (index, array) in batch.columns().iter().enumerate() {
            nulls[index] += array.null_count();
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"path":destination.canonicalize()?,"rows":rows,"columns":columns,"bytes":bytes,"top_level_nulls_by_column":nulls,"elapsed_seconds":started.elapsed().as_secs_f64(),"writer":"Arrow + Parquet","scope":"Stored fixture validation; not a GUI frame-rate measurement"})
        )?
    );
    Ok(())
}
