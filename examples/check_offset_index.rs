//! Paired decoder benchmark, including bytes delivered by ChunkReader.
#[path = "../src/parquet_page_index.rs"]
mod parquet_page_index;

use std::{
    fs::File,
    io::{BufReader, Read},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use arrow::{
    array::{Array, ListArray, StringArray, UInt64Array},
    datatypes::Int64Type,
    record_batch::{RecordBatch, RecordBatchReader},
};
use bytes::Bytes;
use parquet::{
    arrow::{
        ArrowWriter,
        arrow_reader::{
            ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
            RowSelector,
        },
    },
    file::{
        properties::WriterProperties,
        reader::{ChunkReader, Length},
    },
};

const GROUP_ROWS: usize = 262_144;
const SKIP: usize = 240_000;
const COUNT: usize = 1_024;

#[derive(Default)]
struct Reads {
    bytes: AtomicU64,
    calls: AtomicU64,
}

struct Metered(File, Arc<Reads>);
struct MeteredRead(BufReader<File>, Arc<Reads>);

impl Read for MeteredRead {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        let count = self.0.read(data)?;
        self.1.bytes.fetch_add(count as u64, Ordering::Relaxed);
        Ok(count)
    }
}

impl Length for Metered {
    fn len(&self) -> u64 {
        self.0.len()
    }
}

impl ChunkReader for Metered {
    type T = MeteredRead;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        self.1.calls.fetch_add(1, Ordering::Relaxed);
        Ok(MeteredRead(self.0.get_read(start)?, self.1.clone()))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        let data = self.0.get_bytes(start, length)?;
        self.1.calls.fetch_add(1, Ordering::Relaxed);
        self.1.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(data)
    }
}

fn fixture(index: bool) -> Result<File> {
    let file = tempfile::tempfile()?;
    let batch = RecordBatch::try_from_iter([
        (
            "id",
            Arc::new(UInt64Array::from_iter_values(0..GROUP_ROWS as u64 * 2)) as _,
        ),
        (
            "text",
            Arc::new(StringArray::from_iter((0..GROUP_ROWS * 2).map(|row| {
                (row % 7 != 0).then(|| format!("row-{row:06}-{}", "abcdefghijklmnopqrstuvwxyz"))
            }))) as _,
        ),
        (
            "list",
            Arc::new(ListArray::from_iter_primitive::<Int64Type, _, _>(
                (0..GROUP_ROWS * 2).map(|row| (row % 5 != 0).then_some([Some(row as i64), None])),
            )) as _,
        ),
    ])?;
    let options = WriterProperties::builder()
        .set_max_row_group_row_count(Some(GROUP_ROWS))
        .set_data_page_row_count_limit(512)
        .set_dictionary_enabled(false)
        .set_offset_index_disabled(!index)
        .build();
    let mut writer = ArrowWriter::try_new(file.try_clone()?, batch.schema(), Some(options))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(file)
}

fn read_range(
    file: &File,
    metadata: &ArrowReaderMetadata,
    index: bool,
    skip: usize,
    count: usize,
) -> Result<(RecordBatch, serde_json::Value)> {
    let reads = Arc::new(Reads::default());
    let file = Metered(file.try_clone()?, reads.clone());
    let group = 1;
    let start = Instant::now();
    let indexed = index
        .then(|| parquet_page_index::for_range(&file, metadata, group, skip, count))
        .flatten();
    let used = indexed.is_some();
    let index_bytes = reads.bytes.load(Ordering::Relaxed);
    let (metadata, group) = indexed.map_or_else(|| (metadata.clone(), group), |value| (value, 0));
    let selection = RowSelection::from(vec![
        RowSelector::skip(skip),
        RowSelector::select(count),
        RowSelector::skip(GROUP_ROWS - skip - count),
    ]);
    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, metadata)
        .with_row_groups(vec![group])
        .with_row_selection(selection)
        .with_batch_size(8192)
        .build()?;
    let schema = reader.schema();
    let batches = reader.collect::<std::result::Result<Vec<_>, _>>()?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.;
    let rows = arrow::compute::concat_batches(&schema, &batches)?;
    ensure!(rows.num_rows() == count, "Range row count changed");
    let ids = rows
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .context("Expected UInt64")?;
    ensure!(
        !ids.is_null(0) && ids.value(0) == (GROUP_ROWS + skip) as u64,
        "Wrong source group or offset"
    );
    Ok((
        rows,
        serde_json::json!({
            "milliseconds": elapsed,
            "used_offset_index": used,
            "index_bytes": index_bytes,
            "all_bytes_delivered": reads.bytes.load(Ordering::Relaxed),
            "chunk_reader_calls": reads.calls.load(Ordering::Relaxed),
        }),
    ))
}

fn main() -> Result<()> {
    let file = fixture(true)?;
    let metadata = ArrowReaderMetadata::load(&file, ArrowReaderOptions::new())?;
    let mut pairs = Vec::new();
    for trial in 0..7 {
        let (before, after) = if trial % 2 == 0 {
            (
                read_range(&file, &metadata, false, SKIP, COUNT)?,
                read_range(&file, &metadata, true, SKIP, COUNT)?,
            )
        } else {
            let after = read_range(&file, &metadata, true, SKIP, COUNT)?;
            (read_range(&file, &metadata, false, SKIP, COUNT)?, after)
        };
        ensure!(
            before.0 == after.0 && after.1["used_offset_index"] == true,
            "OffsetIndex path must preserve nested/null values"
        );
        pairs.push(serde_json::json!({"before": before.1, "after": after.1}));
    }
    let no_index = fixture(false)?;
    let no_index_metadata = ArrowReaderMetadata::load(&no_index, ArrowReaderOptions::new())?;
    let fallback = read_range(&no_index, &no_index_metadata, true, SKIP, COUNT)?;
    ensure!(
        fallback.1["used_offset_index"] == false,
        "Missing OffsetIndex must fall back"
    );
    ensure!(
        fallback.0 == read_range(&file, &metadata, false, SKIP, COUNT)?.0,
        "Fallback changed values"
    );
    let whole = read_range(&file, &metadata, true, 0, GROUP_ROWS)?;
    ensure!(
        whole.1["used_offset_index"] == false,
        "Full row group must not load OffsetIndex"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "row_group_rows": GROUP_ROWS, "second_group_skip": SKIP, "range_rows": COUNT,
            "pairs": pairs, "missing_index_fallback": fallback.1, "whole_group": whole.1,
            "scope": "ChunkReader-delivered bytes exclude OS or BufReader read-ahead. Timing includes optional index loading and decode, excludes footer and validation.",
        }))?
    );
    Ok(())
}
