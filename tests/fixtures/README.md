The fixture regression generates its own typed Parquet files.

```sh
cargo run --locked --release --example check_fixture_regressions
```

The generated cases cover dates and timestamps, decimals and booleans, binary,
nullable lists, nested structs, maps, case-insensitive field-name collisions,
and all-null Parquet V2 data pages. Inputs and reports live in a new directory
under `target/` for each run.
