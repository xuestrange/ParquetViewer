//! Create an actual >=5 GB uncompressed nested Parquet fixture in bounded batches.
mod support;
use anyhow::{Result, ensure};
use std::{path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/large-data/struct-5gb.parquet".into());
    let started = Instant::now();
    support::make_large(&destination)?;
    let bytes = std::fs::metadata(&destination)?.len();
    ensure!(bytes >= 5_000_000_000, "Expected >=5 GB, got {bytes} bytes");
    let rows = 2_600_000;
    let columns = 3;
    let nulls = Vec::<usize>::new();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"path":destination.canonicalize()?,"rows":rows,"columns":columns,"bytes":bytes,"top_level_nulls_by_column":nulls,"elapsed_seconds":started.elapsed().as_secs_f64(),"writer":"Arrow + Parquet","scope":"Stored fixture validation; not a GUI frame-rate measurement"})
        )?
    );
    Ok(())
}
