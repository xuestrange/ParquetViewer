//! Create the deterministic typed demo, including nested values and image/audio bytes.
mod support;
use anyhow::{Result, ensure};
use std::{path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo/demo.parquet".into());
    let started = Instant::now();
    support::make_demo(&destination)?;
    let bytes = std::fs::metadata(&destination)?.len();
    ensure!(bytes > 0, "Fixture was not written");
    let mut rows = 0usize;
    let mut columns = 0usize;
    let mut nulls = Vec::<usize>::new();
    for batch in support::read_all(&destination)? {
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
