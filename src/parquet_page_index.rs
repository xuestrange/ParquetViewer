//! Load a bounded OffsetIndex only for a small range deep in a row group.
use std::sync::Arc;

use parquet::{
    arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions},
    file::{
        metadata::{PageIndexPolicy, ParquetMetaData, ParquetMetaDataReader},
        reader::ChunkReader,
    },
};

const MIN_ROWS_TO_SKIP: usize = 65_536;
const MAX_INDEX_READ_BYTES: u64 = 2 * 1024 * 1024;

/// A successful result describes only the selected row group, at index zero.
/// Missing or unusable index metadata leaves the ordinary reader available.
pub(crate) fn for_range<R: ChunkReader>(
    file: &R,
    source: &ArrowReaderMetadata,
    group_index: usize,
    skip: usize,
    count: usize,
) -> Option<ArrowReaderMetadata> {
    let metadata = source.metadata();
    let group = metadata.row_group(group_index);
    let rows = usize::try_from(group.num_rows()).ok()?;
    if skip < MIN_ROWS_TO_SKIP || count > rows / 4 {
        return None;
    }

    // The pinned reader fetches the span covering all indexes in this group.
    // Cap that span before requesting any data. Require indexes for all leaves
    // so a partially populated group cannot change decoder behavior.
    let mut start = u64::MAX;
    let mut end = 0;
    for column in group.columns() {
        let offset = u64::try_from(column.offset_index_offset()?).ok()?;
        let length = u64::try_from(column.offset_index_length()?).ok()?;
        if offset < 4 || length == 0 {
            return None;
        }
        start = start.min(offset);
        end = end.max(offset.checked_add(length)?);
    }
    if end > file.len() || end.checked_sub(start)? > MAX_INDEX_READ_BYTES {
        return None;
    }

    let mut reader = ParquetMetaDataReader::new_with_metadata(ParquetMetaData::new(
        metadata.file_metadata().clone(),
        vec![group.clone()],
    ))
    .with_offset_index_policy(PageIndexPolicy::Optional);
    reader.read_page_indexes(file).ok()?;
    let indexed = reader.finish().ok()?;
    let columns = indexed.offset_index()?.first()?;
    if columns.len() != group.num_columns() {
        return None;
    }
    for (index, column) in columns.iter().zip(group.columns()) {
        let pages = index.page_locations();
        let start = u64::try_from(
            column
                .dictionary_page_offset()
                .unwrap_or_else(|| column.data_page_offset()),
        )
        .ok()?;
        let len = u64::try_from(column.compressed_size()).ok()?;
        let end = start.checked_add(len)?;
        let mut previous_end = start;
        let mut previous_row = -1;
        let first = pages.first()?;
        if first.first_row_index != 0 || first.offset != column.data_page_offset() {
            return None;
        }
        for page in pages {
            let offset = u64::try_from(page.offset).ok()?;
            let len = u64::try_from(page.compressed_page_size).ok()?;
            let page_end = offset.checked_add(len)?;
            if len == 0
                || offset < previous_end
                || page_end > end
                || page.first_row_index <= previous_row
                || page.first_row_index >= group.num_rows()
            {
                return None;
            }
            previous_end = page_end;
            previous_row = page.first_row_index;
        }
    }

    ArrowReaderMetadata::try_new(
        Arc::new(indexed),
        ArrowReaderOptions::new().with_schema(source.schema().clone()),
    )
    .ok()
}
