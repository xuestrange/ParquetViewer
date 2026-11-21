//! Same-body field-cache and projected-export benchmark, with real correctness
//! checks. SOURCE [fields|export|export-small], or PV_PROJECTION_BENCH_MODE.
//! `cold` means a fresh application cache; the OS page cache is uncontrolled.
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
use arrow::array::{
    Array, ArrayData, Int32Array, Int64Array, LargeStringArray, StringArray, StringViewArray,
    UInt32Array, UInt64Array,
};
use calamine::{Data, Reader, Xls, Xlsx, open_workbook};
use engine::{Engine, ExportFormat, QueryRequest, QueryResult};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::de::{self, DeserializeSeed, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::{
    fmt,
    fs::{self, File, Metadata},
    io::BufReader,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
struct Order {
    first: usize,
    descending: bool,
}
impl Order {
    fn id(self, row: usize) -> usize {
        if self.descending {
            self.first - row
        } else {
            self.first + row
        }
    }
}
fn column(result: &QueryResult, name: &str) -> Result<usize> {
    result
        .columns
        .iter()
        .position(|column| column.name == name)
        .with_context(|| format!("Missing field {name}"))
}
fn request(fields: &[&str], rows: usize) -> QueryRequest {
    QueryRequest {
        selected_columns: fields.iter().map(|name| (*name).into()).collect(),
        limit: rows.max(1),
        ..Default::default()
    }
}
fn stamp(metadata: &Metadata) -> Result<Value> {
    Ok(
        json!({"bytes":metadata.len(),"device":metadata.dev(),"inode":metadata.ino(),"mtime_ns":metadata.modified()?.duration_since(UNIX_EPOCH)?.as_nanos()}),
    )
}
fn sample_rows(count: usize) -> Vec<usize> {
    let mut rows = vec![
        0,
        count / 2,
        count.saturating_sub(1),
        8191,
        8192,
        32767,
        32768,
    ];
    rows.retain(|row| *row < count);
    rows.sort_unstable();
    rows.dedup();
    rows
}
fn verify(result: &QueryResult, fields: &[&str], rows: usize, order: Order) -> Result<Value> {
    ensure!(result.row_count() == rows, "Wrong result row count");
    ensure!(
        result
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .eq(fields.iter().copied()),
        "Wrong selected field order"
    );
    let id = column(result, "id")?;
    for row in 0..rows {
        let expected = order.id(row);
        ensure!(
            result.cell(row, id).text.parse::<usize>()? == expected,
            "Wrong id at result row {row}"
        );
        ensure!(
            result.source_row_number(row) == Some(expected + 1),
            "Original row number changed at result row {row}"
        );
    }
    let mut anchors = Vec::new();
    for row in sample_rows(rows) {
        let expected = order.id(row);
        if let Some(details) = result.columns.iter().position(|c| c.name == "details") {
            let cell = result.cell(row, details);
            let value = cell.json.context("Missing nested Struct value")?;
            ensure!(
                value["customer"]["id"].as_u64() == Some((expected % 100003) as u64),
                "Wrong nested customer id"
            );
            ensure!(
                value["customer"]["city"].as_str()
                    == Some(if expected % 2 == 0 {
                        "上海"
                    } else {
                        "杭州"
                    }),
                "Wrong nested city"
            );
            ensure!(
                value["measurements"]
                    .as_array()
                    .is_some_and(|values| values.len() == 2),
                "Wrong nested List"
            );
            ensure!(
                value["payload"]
                    .as_str()
                    .is_some_and(|text| text.len() == 2048),
                "Wrong nested payload length"
            );
        }
        if let Some(note) = result.columns.iter().position(|c| c.name == "note") {
            let cell = result.cell(row, note);
            ensure!(
                cell.is_null == (expected % 11 == 0),
                "Wrong note NULL state"
            );
            if !cell.is_null {
                ensure!(cell.text == format!("record {expected}"), "Wrong note text");
            }
        }
        anchors.push(json!({"result_row":row,"id":expected,"source_row":expected+1}));
    }
    Ok(json!({"all_ids_and_source_rows_checked":rows,"typed_nested_and_note_anchors":anchors}))
}
fn buffer_spans(result: &QueryResult, name: &str) -> Vec<(usize, usize)> {
    fn collect(data: &ArrayData, spans: &mut Vec<(usize, usize)>) {
        for buffer in data.buffers() {
            if !buffer.is_empty() {
                let start = buffer.as_ptr() as usize;
                spans.push((start, start.saturating_add(buffer.len())));
            }
        }
        for child in data.child_data() {
            collect(child, spans);
        }
    }
    let Some(index) = result.columns.iter().position(|c| c.name == name) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    for batch in result.data.raw_batches() {
        collect(&batch.column(index).to_data(), &mut spans);
    }
    spans.sort_unstable();
    let mut merged = Vec::<(usize, usize)>::new();
    for (start, end) in spans {
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}
fn shared_buffers(before: &QueryResult, after: &QueryResult) -> Value {
    Value::Array(["id","details","note"].into_iter().filter(|name|before.columns.iter().any(|c|c.name==*name)&&after.columns.iter().any(|c|c.name==*name)).map(|name|{
        let a=buffer_spans(before,name);let b=buffer_spans(after,name);let total=b.iter().map(|(start,end)|end-start).sum::<usize>();
        let(mut i,mut j,mut shared)=(0,0,0usize);
        while i<a.len()&&j<b.len(){shared+=a[i].1.min(b[j].1).saturating_sub(a[i].0.max(b[j].0));if a[i].1<=b[j].1{i+=1;}else{j+=1;}}
        json!({"field":name,"after_arrow_data_buffer_bytes":total,"overlapping_original_buffer_bytes":shared,"all_after_buffers_shared":shared==total,"scope":"Address-range overlap of retained Arrow data/offset buffers; excludes validity and is not RSS or a cache-hit assertion"})
    }).collect())
}
fn query(
    engine: &mut Engine,
    label: &str,
    request: &QueryRequest,
    original: Option<&QueryResult>,
    order: Order,
    rows: usize,
) -> Result<(QueryResult, Value)> {
    let start = Instant::now();
    let result = engine.query(request)?;
    let query_ms = start.elapsed().as_secs_f64() * 1000.;
    let validate_at = Instant::now();
    let fields = request
        .selected_columns
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let correctness = verify(&result, &fields, rows, order)?;
    let sharing = original.map(|old| shared_buffers(old, &result));
    let validation_ms = validate_at.elapsed().as_secs_f64() * 1000.;
    let report = json!({"case":label,"selected_fields":request.selected_columns,"filter":request.filter,"sort":request.sort,"query_milliseconds":query_ms,"engine_reported_elapsed_ms":result.elapsed_ms,"rows":result.row_count(),"loaded_window_rows":result.loaded_rows,"visible_result_logical_arrow_bytes":result.data.memory_size(),"timings":{"window_load_ms":result.timings.window_load_ms,"index_query_ms":result.timings.index_query_ms,"metadata_ms":result.timings.metadata_ms,"arrow_start_ms":result.timings.arrow_start_ms,"arrow_fetch_ms":result.timings.arrow_fetch_ms,"adapter_ms":result.timings.adapter_ms,"window_cache_hit":result.timings.window_cache_hit},"buffer_sharing_with_original":sharing,"validation_milliseconds":validation_ms,"correctness":correctness});
    Ok((result, report))
}
fn publish(
    engine: &mut Engine,
    current: &mut Option<QueryResult>,
    next: QueryResult,
    pinned: &str,
) -> Result<()> {
    if let Some(previous) = current.replace(next)
        && previous.snapshot_id != pinned
    {
        engine.discard_snapshot(&previous.snapshot_id)?;
    }
    Ok(())
}
fn id_value(array: &dyn Array, row: usize) -> Result<usize> {
    ensure!(!array.is_null(row), "Exported id is NULL");
    if let Some(values) = array.as_any().downcast_ref::<Int64Array>() {
        return Ok(usize::try_from(values.value(row))?);
    }
    if let Some(values) = array.as_any().downcast_ref::<Int32Array>() {
        return Ok(usize::try_from(values.value(row))?);
    }
    if let Some(values) = array.as_any().downcast_ref::<UInt64Array>() {
        return Ok(usize::try_from(values.value(row))?);
    }
    if let Some(values) = array.as_any().downcast_ref::<UInt32Array>() {
        return Ok(values.value(row) as usize);
    }
    anyhow::bail!("Fixture id must be an integer")
}
fn note_value(array: &dyn Array, row: usize) -> Result<Option<&str>> {
    if array.is_null(row) {
        return Ok(None);
    }
    if let Some(values) = array.as_any().downcast_ref::<StringArray>() {
        return Ok(Some(values.value(row)));
    }
    if let Some(values) = array.as_any().downcast_ref::<LargeStringArray>() {
        return Ok(Some(values.value(row)));
    }
    if let Some(values) = array.as_any().downcast_ref::<StringViewArray>() {
        return Ok(Some(values.value(row)));
    }
    anyhow::bail!("Fixture note must be text")
}
fn expected_note(snapshot: &QueryResult, row: usize) -> Result<Option<String>> {
    let cell = snapshot.cell(row, column(snapshot, "note")?);
    Ok((!cell.is_null).then_some(cell.text))
}
fn check_json_row(
    value: &Value,
    snapshot: &QueryResult,
    fields: &[String],
    row: usize,
    order: Order,
) -> Result<()> {
    let object = value
        .as_object()
        .context("Exported JSON row is not an object")?;
    ensure!(
        object.len() == fields.len() && fields.iter().all(|name| object.contains_key(name)),
        "JSON exported unexpected/missing columns"
    );
    ensure!(
        value["id"].as_u64() == Some(order.id(row) as u64),
        "JSON export id order changed at row {row}"
    );
    if fields.iter().any(|name| name == "note") {
        let expected = expected_note(snapshot, row)?;
        ensure!(
            value["note"].as_str() == expected.as_deref()
                && value["note"].is_null() == expected.is_none(),
            "JSON note changed at row {row}"
        );
    }
    Ok(())
}
struct JsonRows<'a> {
    snapshot: &'a QueryResult,
    fields: &'a [String],
    order: Order,
}
impl<'de> DeserializeSeed<'de> for JsonRows<'_> {
    type Value = usize;
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<usize, D::Error> {
        deserializer.deserialize_seq(self)
    }
}
impl<'de> Visitor<'de> for JsonRows<'_> {
    type Value = usize;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an array of exported rows")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<usize, A::Error> {
        let mut rows = 0;
        while let Some(value) = sequence.next_element::<Value>()? {
            if rows >= self.snapshot.row_count() {
                return Err(de::Error::custom("JSON contains excess records"));
            }
            check_json_row(&value, self.snapshot, self.fields, rows, self.order)
                .map_err(de::Error::custom)?;
            rows += 1;
        }
        Ok(rows)
    }
}
fn validate_parquet(
    path: &Path,
    snapshot: &QueryResult,
    fields: &[String],
    order: Order,
) -> Result<usize> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    ensure!(
        reader
            .schema()
            .fields()
            .iter()
            .map(|field| field.name())
            .eq(fields.iter()),
        "Parquet exported wrong field order"
    );
    let mut rows = 0;
    for batch in reader.with_batch_size(8192).build()? {
        let batch = batch?;
        for row in 0..batch.num_rows() {
            ensure!(
                rows < snapshot.row_count(),
                "Parquet contains excess records"
            );
            ensure!(
                id_value(batch.column(0).as_ref(), row)? == order.id(rows),
                "Parquet id order changed at row {rows}"
            );
            if fields.len() == 2 {
                let expected = expected_note(snapshot, rows)?;
                ensure!(
                    note_value(batch.column(1).as_ref(), row)? == expected.as_deref(),
                    "Parquet note changed at row {rows}"
                );
            }
            rows += 1;
        }
    }
    Ok(rows)
}
fn validate_excel(
    path: &Path,
    format: ExportFormat,
    snapshot: &QueryResult,
    fields: &[String],
    order: Order,
) -> Result<usize> {
    let range = if format == ExportFormat::Xlsx {
        let mut workbook: Xlsx<_> = open_workbook(path)?;
        workbook.worksheet_range("Parquet data")?
    } else {
        let mut workbook: Xls<_> = open_workbook(path)?;
        workbook.worksheet_range("Parquet data")?
    };
    ensure!(
        range.height() == snapshot.row_count() + 1 && range.width() == fields.len(),
        "Excel dimensions changed"
    );
    for (column, name) in fields.iter().enumerate() {
        ensure!(
            range.get((0, column)) == Some(&Data::String(name.clone())),
            "Excel header order changed"
        );
    }
    for row in 0..snapshot.row_count() {
        let expected = order.id(row);
        ensure!(
            matches!(range.get((row+1,0)),Some(Data::Float(value)) if *value==expected as f64)
                || matches!(range.get((row+1,0)),Some(Data::Int(value)) if *value==expected as i64),
            "Excel id order changed"
        );
        if fields.len() == 2 {
            match expected_note(snapshot, row)? {
                Some(text) => ensure!(
                    range.get((row + 1, 1)) == Some(&Data::String(text)),
                    "Excel note changed"
                ),
                None => ensure!(
                    matches!(range.get((row + 1, 1)), None | Some(Data::Empty)),
                    "Excel NULL note changed"
                ),
            }
        }
    }
    Ok(snapshot.row_count())
}
fn export_case(
    engine: &Engine,
    label: &str,
    snapshot: &QueryResult,
    fields: &[&str],
    format: ExportFormat,
    output: &Path,
    order: Order,
) -> Result<Value> {
    let extension = match format {
        ExportFormat::Csv => "csv",
        ExportFormat::Parquet => "parquet",
        ExportFormat::Json => "json",
        ExportFormat::Xlsx => "xlsx",
        ExportFormat::Xls => "xls",
    };
    let fields = fields
        .iter()
        .map(|name| (*name).into())
        .collect::<Vec<String>>();
    let path = output.join(format!("{label}.{extension}"));
    let start = Instant::now();
    engine.export_result_columns_cancellable(
        &snapshot.snapshot_id,
        &fields,
        &path,
        format,
        &AtomicBool::new(false),
    )?;
    let export_ms = start.elapsed().as_secs_f64() * 1000.;
    let at = Instant::now();
    let rows = match format {
        ExportFormat::Csv => {
            let mut reader = csv::Reader::from_path(&path)?;
            ensure!(
                reader
                    .headers()?
                    .iter()
                    .eq(fields.iter().map(String::as_str)),
                "CSV field order changed"
            );
            let mut rows = 0;
            for record in reader.records() {
                let record = record?;
                ensure!(rows < snapshot.row_count(), "CSV contains extra records");
                let id = order.id(rows).to_string();
                ensure!(record.get(0) == Some(id.as_str()), "CSV id order changed");
                if fields.len() == 2 {
                    let note = expected_note(snapshot, rows)?;
                    ensure!(
                        record.get(1) == Some(note.as_deref().unwrap_or("")),
                        "CSV note changed"
                    );
                }
                rows += 1;
            }
            rows
        }
        ExportFormat::Parquet => validate_parquet(&path, snapshot, &fields, order)?,
        ExportFormat::Json => {
            let mut reader =
                serde_json::Deserializer::from_reader(BufReader::new(File::open(&path)?));
            let rows = JsonRows {
                snapshot,
                fields: &fields,
                order,
            }
            .deserialize(&mut reader)?;
            reader.end()?;
            rows
        }
        ExportFormat::Xlsx | ExportFormat::Xls => {
            validate_excel(&path, format, snapshot, &fields, order)?
        }
    };
    ensure!(rows == snapshot.row_count(), "Export changed record count");
    Ok(
        json!({"case":label,"format":extension,"fields":fields,"rows":rows,"export_milliseconds":export_ms,"validation_milliseconds":at.elapsed().as_secs_f64()*1000.,"output_bytes":fs::metadata(&path)?.len(),"all_exported_id_note_values_and_order_checked":true,"path":path}),
    )
}
fn fields_workload(mut engine: Engine, total: usize, output: &Path) -> Result<Value> {
    let ascending = Order {
        first: 0,
        descending: false,
    };
    let mut queries = Vec::new();
    let (cold, report) = query(
        &mut engine,
        "cold_id_details",
        &request(&["id", "details"], total),
        None,
        ascending,
        total,
    )?;
    queries.push(report);
    let mut current = Some(cold.clone());
    for (label, fields) in [
        ("subset_id", vec!["id"]),
        ("reorder_details_id", vec!["details", "id"]),
        ("add_note", vec!["details", "id", "note"]),
        ("hide_details_note", vec!["id"]),
        ("reselect_hidden_fields", vec!["details", "id", "note"]),
    ] {
        let (next, report) = query(
            &mut engine,
            label,
            &request(&fields, total),
            Some(&cold),
            ascending,
            total,
        )?;
        queries.push(report);
        publish(&mut engine, &mut current, next, &cold.snapshot_id)?;
    }
    let filtered_rows = 1000.min(total - total / 2);
    let first = total / 2 + filtered_rows - 1;
    let mut filtered = request(&["details", "id", "note"], total);
    filtered.filter = format!(
        "\"id\" >= {} AND \"id\" < {}",
        total / 2,
        total / 2 + filtered_rows
    );
    filtered.sort = Some(("id".into(), true));
    let (next, report) = query(
        &mut engine,
        "filter_sort_after_field_changes",
        &filtered,
        Some(&cold),
        Order {
            first,
            descending: true,
        },
        filtered_rows,
    )?;
    queries.push(report);
    publish(&mut engine, &mut current, next, &cold.snapshot_id)?;
    verify(&cold, &["id", "details"], total, ascending)?;
    let original_export = export_case(
        &engine,
        "original_snapshot_id",
        &cold,
        &["id"],
        ExportFormat::Parquet,
        output,
        ascending,
    )?;
    Ok(
        json!({"queries":queries,"original_snapshot_export":original_export,"snapshot_retention":"Original id/details snapshot is pinned through the sequence; only it and the current display survive publication. The previous display stays alive while its replacement loads, matching the application."}),
    )
}
fn export_workload(mut engine: Engine, total: usize, output: &Path, small: bool) -> Result<Value> {
    let loaded = if small { total.min(5000) } else { total };
    let exported = if small { loaded } else { loaded.min(100_000) };
    let (full, load) = query(
        &mut engine,
        "load_full_fields",
        &request(&["id", "details", "note"], loaded),
        None,
        Order {
            first: 0,
            descending: false,
        },
        loaded,
    )?;
    let mut requested = request(&["id", "details", "note"], loaded);
    requested.filter = format!("\"id\" < {exported}");
    requested.sort = Some(("id".into(), true));
    let order = Order {
        first: exported - 1,
        descending: true,
    };
    let (sorted, sort) = query(
        &mut engine,
        "index_view_sort",
        &requested,
        Some(&full),
        order,
        exported,
    )?;
    let formats = if small {
        vec![
            ExportFormat::Parquet,
            ExportFormat::Json,
            ExportFormat::Xlsx,
            ExportFormat::Xls,
        ]
    } else {
        vec![ExportFormat::Parquet, ExportFormat::Json]
    };
    let mut exports = Vec::new();
    for format in formats {
        for (label, fields) in [("id_only", vec!["id"]), ("id_note", vec!["id", "note"])] {
            exports.push(export_case(
                &engine, label, &sorted, &fields, format, output, order,
            )?);
        }
    }
    verify(
        &full,
        &["id", "details", "note"],
        loaded,
        Order {
            first: 0,
            descending: false,
        },
    )?;
    Ok(
        json!({"queries":[load,sort],"exports":exports,"loaded_rows":loaded,"exported_rows":exported,"snapshot_retention":"Unfiltered loaded snapshot and sorted index view share the original Arrow storage; export does not replace either."}),
    )
}
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().context(
        "Usage: check_projection_performance SOURCE.parquet [fields|export|export-small]",
    )?);
    let environment_mode = std::env::var("PV_PROJECTION_BENCH_MODE").ok();
    let mode = args
        .next()
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("Benchmark mode must be UTF-8"))
        })
        .transpose()?
        .or_else(|| environment_mode.clone())
        .unwrap_or_else(|| "fields".into());
    ensure!(
        matches!(mode.as_str(), "fields" | "export" | "export-small"),
        "Unknown benchmark mode {mode}"
    );
    let before = stamp(&fs::metadata(&source)?)?;
    let started = Instant::now();
    let at = Instant::now();
    let engine = Engine::open_cancellable(std::slice::from_ref(&source), &AtomicBool::new(false))?;
    let open_ms = at.elapsed().as_secs_f64() * 1000.;
    ensure!(
        engine.info.row_count_known && engine.info.row_count >= 2,
        "Use the sequential id/details/note Parquet fixture with at least two rows"
    );
    for name in ["id", "details", "note"] {
        ensure!(
            engine.info.columns.iter().any(|column| column.name == name),
            "Missing fixture field {name}"
        );
    }
    let total = engine.info.row_count;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let output = PathBuf::from(format!(
        "target/projection-benchmark-{mode}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&output)?;
    let workload = if mode == "fields" {
        fields_workload(engine, total, &output)?
    } else {
        export_workload(engine, total, &output, mode == "export-small")?
    };
    let after = stamp(&fs::metadata(&source)?)?;
    ensure!(
        before == after,
        "Read-only source identity/size/mtime changed"
    );
    let report = json!({"protocol":"same body and case ordering for the pre-optimization and optimized engines; no version-specific sharing assertions","mode":mode,"environment_mode":environment_mode,"source":source.canonicalize()?,"source_rows":total,"source_stamp_before":before,"source_stamp_after":after,"source_metadata_unchanged":true,"source_hash_policy":"No full-file SHA inside the workload; verify a shared fixture hash externally once when required.","open_milliseconds":open_ms,"workload":workload,"in_process_wall_seconds":started.elapsed().as_secs_f64(),"output_directory":output,"scope":"Per-case query/export timings exclude correctness validation. Whole-process wall/CPU/peak metrics include startup, validation, pinned snapshot lifetimes and teardown. In export mode the import peak may dominate whole-process RSS; these metrics do not establish a projection-only memory reduction. Cold means fresh application cache, not a cold OS page cache."});
    source_guard::save_copy(
        &output.join("report.json"),
        &[],
        serde_json::to_string_pretty(&report)?.as_bytes(),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
