//! Check typed Parquet import, sorting and round trips. With no input directory,
//! create deterministic test data beneath a newly created target directory.
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
#[path = "support/regression_fixtures.rs"]
mod regression_fixtures;
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
use arrow::{
    array::{Array, ArrayRef, StringArray},
    compute::{SortOptions, cast},
    datatypes::DataType,
    record_batch::RecordBatch,
    row::{OwnedRow, RowConverter, SortField},
};
use engine::{Engine, ExportFormat, QueryRequest, QueryResult};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_UNCOMPRESSED_BYTES: i64 = 512 * 1024 * 1024;
const MAX_ROWS: usize = 1_000_000;

fn sha256(path: &Path) -> Result<String> {
    let output = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()?;
    ensure!(
        output.status.success(),
        "shasum failed for {}",
        path.display()
    );
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .context("Missing SHA256")?
        .to_owned())
}
fn files(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files(&entry.path(), output)?;
        } else if kind.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
        {
            output.push(entry.path());
        }
    }
    Ok(())
}
fn columns(result: &QueryResult) -> Value {
    Value::Array(result.columns.iter().map(|column|json!({"name":column.name,"type":column.data_type,"nullable":column.nullable})).collect())
}
fn cells(result: &QueryResult, row: usize) -> Value {
    Value::Array(
        (0..result.columns.len())
            .map(|column| {
                let cell = result.cell(row, column);
                json!({"text":cell.text,"is_null":cell.is_null,"bytes":cell.bytes,"json":cell.json})
            })
            .collect(),
    )
}
fn anchors(result: &QueryResult) -> Value {
    let mut rows = if result.is_empty() {
        Vec::new()
    } else {
        vec![0, result.row_count() / 2, result.row_count() - 1]
    };
    rows.sort_unstable();
    rows.dedup();
    Value::Array(rows.into_iter().map(|row|json!({"result_row":row,"source_row":result.source_row_number(row),"cells":cells(result,row)})).collect())
}
fn dump_cells(result: &QueryResult, path: &Path) -> Result<()> {
    let mut writer = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(path)?);
    serde_json::to_writer(
        &mut writer,
        &json!({"columns":columns(result),"rows":result.row_count(),"arrow_schema":format!("{:?}",result.data.schema())}),
    )?;
    writer.write_all(b"\n")?;
    for row in 0..result.row_count() {
        serde_json::to_writer(
            &mut writer,
            &json!({"source_row":result.source_row_number(row),"cells":cells(result,row)}),
        )?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

struct Rows<'a> {
    batches: Box<dyn Iterator<Item = Result<RecordBatch>> + 'a>,
    current: Option<RecordBatch>,
    next: usize,
    formatters: Vec<columnar::ExportFormatter>,
}
impl<'a> Rows<'a> {
    fn new(result: &'a QueryResult, cancel: &'a AtomicBool) -> Self {
        Self {
            batches: Box::new(result.data.export_batches(8192, cancel)),
            current: None,
            next: 0,
            formatters: result
                .columns
                .iter()
                .map(|c| columnar::ExportFormatter::new(&c.data_type))
                .collect(),
        }
    }
    fn next(&mut self) -> Result<Option<Vec<Value>>> {
        if self
            .current
            .as_ref()
            .is_none_or(|batch| self.next >= batch.num_rows())
        {
            self.current = self.batches.next().transpose()?;
            self.next = 0;
        }
        let Some(batch) = &self.current else {
            return Ok(None);
        };
        let values = self
            .formatters
            .iter()
            .enumerate()
            .map(|(column, formatter)| {
                formatter.json_value(batch.column(column).as_ref(), self.next)
            })
            .collect();
        self.next += 1;
        Ok(Some(values))
    }
}
fn roundtrip_equal(original: &QueryResult, reopened: &QueryResult) -> Result<usize> {
    ensure!(
        original.row_count() == reopened.row_count(),
        "Parquet export/reopen changed row count"
    );
    ensure!(
        original.columns == reopened.columns,
        "Parquet export/reopen changed presentation schema: {:?} != {:?}",
        original.columns,
        reopened.columns
    );
    ensure!(
        original.data.schema() == reopened.data.schema(),
        "Parquet export/reopen changed Arrow schema: {:?} != {:?}",
        original.data.schema(),
        reopened.data.schema()
    );
    let cancel = AtomicBool::new(false);
    let mut left = Rows::new(original, &cancel);
    let mut right = Rows::new(reopened, &cancel);
    for row in 0..original.row_count() {
        let left = left.next()?.context("Original Arrow stream ended early")?;
        let right = right.next()?.context("Reopened Arrow stream ended early")?;
        if left != right {
            let column = left.iter().zip(&right).position(|(a, b)| a != b).unwrap();
            anyhow::bail!(
                "Parquet export/reopen changed row {row}, field {}: {} != {}",
                original.columns[column].name,
                left[column],
                right[column]
            );
        }
        // Cell includes raw bytes and the actual Preview's exact text/tree.
        ensure!(
            cells(original, row) == cells(reopened, row),
            "Parquet export/reopen changed typed Cell at row {row}"
        );
    }
    ensure!(
        left.next()?.is_none() && right.next()?.is_none(),
        "Arrow stream contains unexpected extra rows"
    );
    Ok(original.row_count().saturating_mul(original.columns.len()))
}
fn key_array(
    field: &arrow::datatypes::Field,
    array: &ArrayRef,
    case_insensitive: bool,
) -> Result<ArrayRef> {
    if case_insensitive {
        let array = cast(array, &DataType::Utf8)?;
        let array = array.as_any().downcast_ref::<StringArray>().unwrap();
        // Independent validator only: the production engine uses DataFusion's
        // lower expression. This normalizes one small fixture batch at a time.
        return Ok(Arc::new(StringArray::from_iter(
            array.iter().map(|value| value.map(str::to_lowercase)),
        )));
    }
    columnar::sortable_array(&parquet_compat::interval_sort_array(field, array)?)
}
fn verify_sort(
    original: &QueryResult,
    sorted: &QueryResult,
    column: usize,
    descending: bool,
) -> Result<Value> {
    ensure!(
        sorted.row_count() == original.row_count(),
        "Sort changed record count"
    );
    ensure!(sorted.columns == original.columns, "Sort changed columns");
    let count = original.row_count();
    let mut seen = vec![false; count];
    for row in 0..count {
        let source = sorted
            .source_row_number(row)
            .context("Sort lost original row number")?;
        ensure!(
            (1..=count).contains(&source),
            "Sort returned out-of-range original row number {source}"
        );
        ensure!(
            !std::mem::replace(&mut seen[source - 1], true),
            "Sort duplicated original row number {source}"
        );
        let actual = sorted.cell(row, column);
        let expected = original.cell(source - 1, column);
        ensure!(
            actual.text == expected.text
                && actual.is_null == expected.is_null
                && actual.bytes == expected.bytes
                && actual.json == expected.json,
            "Sort returned mismatched value for source row {source}, field {}",
            original.columns[column].name
        );
    }
    ensure!(seen.iter().all(|seen| *seen), "Sort lost original rows");
    for anchor in if count == 0 {
        Vec::new()
    } else {
        vec![0, count / 2, count - 1]
    } {
        let source = sorted.source_row_number(anchor).unwrap();
        ensure!(
            cells(sorted, anchor) == cells(original, source - 1),
            "Sort changed an anchor row's payload"
        );
    }
    let case_insensitive = matches!(
        original.columns[column]
            .data_type
            .to_ascii_uppercase()
            .as_str(),
        "VARCHAR" | "TEXT" | "STRING"
    );
    let cancel = AtomicBool::new(false);
    let mut converter = None;
    let mut previous: Option<OwnedRow> = None;
    let mut previous_source = 0usize;
    let mut position = 0usize;
    for batch in sorted.data.export_batches(8192, &cancel) {
        let batch = batch?;
        let key = key_array(
            batch.schema().field(column),
            batch.column(column),
            case_insensitive,
        )?;
        if converter.is_none() {
            converter = Some(RowConverter::new(vec![SortField::new_with_options(
                key.data_type().clone(),
                SortOptions {
                    descending,
                    nulls_first: false,
                },
            )])?);
        }
        let rows = converter.as_mut().unwrap().convert_columns(&[key])?;
        for encoded in rows.iter() {
            let source_row = sorted.source_row_number(position).unwrap();
            if let Some(previous) = &previous {
                ensure!(
                    previous.row() <= encoded,
                    "Sort key is not monotonic for {}",
                    original.columns[column].name
                );
                if previous.row() == encoded {
                    ensure!(
                        previous_source < source_row,
                        "Equal sort keys changed their original row order for {}",
                        original.columns[column].name
                    );
                }
            }
            previous = Some(encoded.owned());
            previous_source = source_row;
            position += 1;
        }
    }
    Ok(
        json!({"direction":if descending{"descending"}else{"ascending"},"rows":count,"original_row_numbers_exact_permutation":true,"key_payloads_exact":true,"arrow_row_order_monotonic":true}),
    )
}

fn check(path: &Path, output: &Path) -> Result<Value> {
    let started = Instant::now();
    let before = fs::metadata(path)?;
    let before_hash = sha256(path)?;
    let metadata = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    let uncompressed = metadata
        .metadata()
        .row_groups()
        .iter()
        .flat_map(|group| group.columns())
        .map(|column| column.uncompressed_size())
        .sum::<i64>();
    let raw_schema = format!("{:?}", metadata.schema());
    ensure!(
        uncompressed <= MAX_UNCOMPRESSED_BYTES,
        "Uncompressed fixture estimate {uncompressed} exceeds safe full-read budget"
    );
    let raw_rows = usize::try_from(metadata.metadata().file_metadata().num_rows())?;
    ensure!(
        raw_rows <= MAX_ROWS,
        "Fixture has {raw_rows} rows, exceeding safe full-read limit"
    );
    drop(metadata);
    let cancel = AtomicBool::new(false);
    let mut engine = Engine::open_cancellable(&[path.to_owned()], &cancel)?;
    ensure!(
        engine.info.row_count_known,
        "Parquet source row count is unknown"
    );
    let request = QueryRequest {
        limit: engine.info.row_count.max(1),
        ..Default::default()
    };
    let original = engine.query(&request)?;
    ensure!(
        original.row_count() == raw_rows,
        "Full read changed source row count"
    );
    for row in 0..raw_rows {
        ensure!(
            original.source_row_number(row) == Some(row + 1),
            "Full read lost original row identity"
        );
    }
    let stem = path
        .file_stem()
        .context("Fixture has no name")?
        .to_string_lossy();
    let golden = output.join(format!("{stem}.original.jsonl"));
    dump_cells(&original, &golden)?;
    let destination = output.join(format!("{stem}.roundtrip.parquet"));
    engine.export_result_columns_cancellable(
        &original.snapshot_id,
        &[],
        &destination,
        ExportFormat::Parquet,
        &cancel,
    )?;
    let mut reader = Engine::open_cancellable(std::slice::from_ref(&destination), &cancel)?;
    let reopened = reader.query(&QueryRequest {
        limit: raw_rows.max(1),
        ..Default::default()
    })?;
    let cells_checked = roundtrip_equal(&original, &reopened)?;
    let mut sorts = Vec::new();
    let mut sort_errors = Vec::new();
    for (column, info) in original.columns.iter().enumerate() {
        for descending in [false, true] {
            let result = engine.query(&QueryRequest {
                sort: Some((info.name.clone(), descending)),
                ..request.clone()
            });
            match result {
                Ok(sorted)=>{
                    let outcome=verify_sort(&original,&sorted,column,descending);
                    engine.discard_snapshot(&sorted.snapshot_id)?;
                    match outcome{Ok(details)=>sorts.push(json!({"field":info.name,"type":info.data_type,"check":details})),Err(error)=>sort_errors.push(json!({"field":info.name,"type":info.data_type,"descending":descending,"stage":"sort_validation","error":format!("{error:#}")}))}
                }
                Err(error)=>sort_errors.push(json!({"field":info.name,"type":info.data_type,"descending":descending,"stage":"sort_query","error":format!("{error:#}")})),
            }
        }
    }
    let after = fs::metadata(path)?;
    ensure!(
        before.len() == after.len()
            && before.modified()? == after.modified()?
            && before_hash == sha256(path)?,
        "Read-only fixture source changed"
    );
    Ok(
        json!({"path":path,"bytes":before.len(),"sha256":before_hash,"source_unchanged":true,"rows":raw_rows,"uncompressed_bytes":uncompressed,"columns":columns(&original),"arrow_schema":format!("{:?}",original.data.schema()),"raw_parquet_arrow_schema":raw_schema,"excluded_case_conflicts":engine.info.excluded_fields,"warnings":engine.info.warnings,"anchors":anchors(&original),"all_cells_compared":cells_checked,"parquet_roundtrip_exact":true,"cell_dump":golden,"roundtrip":destination,"sort_checks":sorts,"sort_errors":sort_errors,"passed":sort_errors.is_empty(),"elapsed_seconds":started.elapsed().as_secs_f64()}),
    )
}
fn main() -> Result<()> {
    let input_directory = std::env::args_os().nth(1).map(PathBuf::from);
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let output = PathBuf::from(format!(
        "target/fixture-regressions-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all("target")?;
    fs::create_dir(&output)?;
    let fixture_dir = match input_directory {
        Some(path) => path,
        None => {
            let path = output.join("fixtures");
            regression_fixtures::generate(&path)?;
            path
        }
    };
    let mut paths = Vec::new();
    files(&fixture_dir, &mut paths)?;
    paths.sort();
    ensure!(!paths.is_empty(), "No Parquet fixtures found");
    let mut reports = Vec::new();
    let mut skipped = 0;
    let mut failed = 0;
    for path in paths {
        let bytes = fs::metadata(&path)?.len();
        if bytes > MAX_SOURCE_BYTES {
            skipped += 1;
            reports.push(json!({"path":path,"bytes":bytes,"skipped":true,"reason":"source exceeds 100 MiB; not counted as passed"}));
            continue;
        }
        match check(&path, &output) {
            Ok(report) => {
                if report["passed"] != true {
                    failed += 1;
                }
                reports.push(report);
            }
            Err(error) => {
                failed += 1;
                reports.push(
                    json!({"path":path,"bytes":bytes,"passed":false,"error":format!("{error:#}")}),
                );
            }
        }
    }
    let passed = reports.len() - skipped - failed;
    let report = json!({"scope":"Parquet import, exact typed-value and schema round trips, and bidirectional sort order and original row identity for every field.","fixtures":reports,"passed":passed,"failed":failed,"skipped":skipped,"output_directory":output});
    let report_path = output.join("report.json");
    source_guard::save_copy(
        &report_path,
        &[],
        serde_json::to_string_pretty(&report)?.as_bytes(),
    )?;
    println!(
        "{}",
        json!({"report":report_path,"passed":passed,"failed":failed,"skipped":skipped})
    );
    ensure!(
        failed == 0,
        "Fixture regression failures: {failed}; see {}",
        report_path.display()
    );
    Ok(())
}
