//! Create a typed 100,000-row NULL-density Parquet fixture.
mod support;
use anyhow::{Result, ensure};
use arrow::array::StringArray;
use std::{path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo/null-density.parquet".into());
    let started = Instant::now();
    support::make_null(&destination)?;
    let bytes = std::fs::metadata(&destination)?.len();
    ensure!(bytes > 0, "Fixture was not written");
    let mut rows = 0usize;
    let mut columns = 0usize;
    let mut nulls = Vec::<usize>::new();
    let mut literals = [0usize; 5];
    for batch in support::read_all(&destination)? {
        rows += batch.num_rows();
        columns = batch.num_columns();
        if nulls.is_empty() {
            nulls.resize(columns, 0);
        }
        for (index, array) in batch.columns().iter().enumerate() {
            nulls[index] += array.null_count();
        }
        for value in batch
            .column(7)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .iter()
        {
            let index = match value {
                Some("") => 0,
                Some("NULL") => 1,
                Some("NaN") => 2,
                Some("null") => 3,
                Some("ordinary text") => 4,
                _ => anyhow::bail!("Literal control changed"),
            };
            literals[index] += 1;
        }
    }
    ensure!(
        rows == 100_000
            && columns == 8
            && nulls == [0, 95_000, 95_652, 96_551, 96_774, 97_297, 97_560, 0],
        "Stored NULL-density controls changed"
    );
    ensure!(
        literals == [20_000; 5],
        "NULL-looking strings were changed into missing values"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"path":destination.canonicalize()?,"rows":rows,"columns":columns,"bytes":bytes,"top_level_nulls_by_column":nulls,"elapsed_seconds":started.elapsed().as_secs_f64(),"writer":"Arrow + Parquet","scope":"Stored fixture validation; not a GUI frame-rate measurement"})
        )?
    );
    Ok(())
}
