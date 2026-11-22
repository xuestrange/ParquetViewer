//! Owned typed Arrow batches. Formatting and JSON are explicit, per-cell work.
use crate::engine::{Cell, ColumnInfo};
use anyhow::{Context, Result, ensure};
use arrow::{
    array::*,
    datatypes::{DataType, TimeUnit},
    record_batch::RecordBatch,
    util::display::{ArrayFormatter, FormatOptions, array_value_to_string},
};
use serde_json::Value;
use std::sync::{Arc, OnceLock};

/// Arrow's global row sorter compares Lists but does not accept Map containers.
/// The entries of a Map have the same lexicographic order as a List of its
/// key/value Structs, so only the container type needs to change for sort keys.
pub(crate) fn sortable_type(data_type: &DataType) -> DataType {
    let field = |field: &arrow::datatypes::FieldRef| {
        Arc::new(
            field
                .as_ref()
                .clone()
                .with_data_type(sortable_type(field.data_type())),
        )
    };
    match data_type {
        DataType::Map(entries, _) => DataType::List(field(entries)),
        DataType::List(child) => DataType::List(field(child)),
        DataType::LargeList(child) => DataType::LargeList(field(child)),
        DataType::ListView(child) => DataType::ListView(field(child)),
        DataType::LargeListView(child) => DataType::LargeListView(field(child)),
        DataType::FixedSizeList(child, size) => DataType::FixedSizeList(field(child), *size),
        DataType::Struct(fields) => DataType::Struct(fields.iter().map(field).collect()),
        DataType::Dictionary(keys, values) => {
            DataType::Dictionary(keys.clone(), Box::new(sortable_type(values)))
        }
        DataType::Union(fields, mode) => DataType::Union(
            fields
                .iter()
                .map(|(id, child)| (id, field(child)))
                .collect(),
            *mode,
        ),
        DataType::RunEndEncoded(runs, values) => {
            DataType::RunEndEncoded(runs.clone(), field(values))
        }
        _ => data_type.clone(),
    }
}

/// Share every value, offsets, validity and dictionary buffer with the source.
/// Only Arrow container metadata is rebuilt, including Maps nested in containers.
pub(crate) fn sortable_array(array: &ArrayRef) -> Result<ArrayRef> {
    fn retype(data: arrow::array::ArrayData) -> Result<arrow::array::ArrayData> {
        let data_type = sortable_type(data.data_type());
        if &data_type == data.data_type() {
            return Ok(data);
        }
        let children = data
            .child_data()
            .iter()
            .cloned()
            .map(retype)
            .collect::<Result<Vec<_>>>()?;
        Ok(data
            .into_builder()
            .data_type(data_type)
            .child_data(children)
            .build()?)
    }
    if sortable_type(array.data_type()) == *array.data_type() {
        return Ok(array.clone());
    }
    Ok(make_array(retype(array.to_data())?))
}

#[derive(Clone, Debug, Default)]
pub struct ArrowTable {
    data: Arc<TableData>,
    // Zero-based global positions in data, never positions in another view.
    indices: Option<Arc<RowIndices>>,
}

/// Width of positions inside the loaded window. Source row numbers are separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowIndexWidth {
    Compact,
    Wide,
}

impl RowIndexWidth {
    pub(crate) fn for_rows(rows: usize) -> Self {
        if u32::try_from(rows.saturating_sub(1)).is_ok() {
            Self::Compact
        } else {
            Self::Wide
        }
    }

    pub(crate) fn data_type(self) -> DataType {
        match self {
            Self::Compact => DataType::UInt32,
            Self::Wide => DataType::UInt64,
        }
    }

    pub(crate) fn positions(self, positions: impl Iterator<Item = u64>) -> RowIndexChunk {
        match self {
            Self::Compact => {
                RowIndexChunk::Compact(UInt32Array::from_iter_values(positions.map(|position| {
                    debug_assert!(u32::try_from(position).is_ok());
                    position as u32
                })))
            }
            Self::Wide => RowIndexChunk::Wide(UInt64Array::from_iter_values(positions)),
        }
    }

    pub(crate) fn contiguous_positions(self, positions: std::ops::Range<usize>) -> RowIndexChunk {
        debug_assert!(
            self == Self::Wide || positions.is_empty() || u32::try_from(positions.end - 1).is_ok()
        );
        RowIndexChunk::Range {
            positions,
            width: self,
        }
    }

    /// Build the final typed position buffer directly from set bits. The
    /// caller has already counted the matches, so sparse masks allocate only
    /// enough space for the retained rows. A single contiguous run is stored
    /// by its endpoints, with no per-row allocation.
    pub(crate) fn matching_positions(
        self,
        start: usize,
        mask: &arrow::buffer::BooleanBuffer,
        matches: usize,
    ) -> RowIndexChunk {
        if let Some((first, end)) = mask.set_slices().next()
            && end - first == matches
        {
            return self.contiguous_positions(start + first..start + end);
        }
        match self {
            Self::Compact => {
                let mut positions = Vec::with_capacity(matches);
                positions.extend(mask.set_indices().map(|row| (start + row) as u32));
                RowIndexChunk::Compact(UInt32Array::from(positions))
            }
            Self::Wide => {
                let mut positions = Vec::with_capacity(matches);
                positions.extend(mask.set_indices().map(|row| (start + row) as u64));
                RowIndexChunk::Wide(UInt64Array::from(positions))
            }
        }
    }

    /// A typed clone retains the streamed Arrow buffer without copying values.
    pub(crate) fn retain_chunk(self, array: &dyn Array) -> Result<RowIndexChunk> {
        let array = array.as_any();
        match self {
            Self::Compact => array
                .downcast_ref::<UInt32Array>()
                .cloned()
                .map(RowIndexChunk::Compact),
            Self::Wide => array
                .downcast_ref::<UInt64Array>()
                .cloned()
                .map(RowIndexChunk::Wide),
        }
        .context("DataFusion returned an invalid row-index type")
    }
}

#[derive(Debug)]
pub(crate) enum RowIndexChunk {
    Compact(UInt32Array),
    Wide(UInt64Array),
    Range {
        positions: std::ops::Range<usize>,
        width: RowIndexWidth,
    },
}

impl From<UInt32Array> for RowIndexChunk {
    fn from(array: UInt32Array) -> Self {
        Self::Compact(array)
    }
}

impl From<UInt64Array> for RowIndexChunk {
    fn from(array: UInt64Array) -> Self {
        Self::Wide(array)
    }
}

impl RowIndexChunk {
    fn data_type(&self) -> DataType {
        match self {
            Self::Compact(_) => DataType::UInt32,
            Self::Wide(_) => DataType::UInt64,
            Self::Range { width, .. } => width.data_type(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Compact(array) => array.len(),
            Self::Wide(array) => array.len(),
            Self::Range { positions, .. } => positions.len(),
        }
    }

    pub(crate) fn into_array(self) -> ArrayRef {
        match self {
            Self::Compact(array) => Arc::new(array),
            Self::Wide(array) => Arc::new(array),
            Self::Range { positions, width } => width
                .positions(positions.map(|row| row as u64))
                .into_array(),
        }
    }

    fn buffer(&self) -> Option<&arrow::buffer::Buffer> {
        match self {
            Self::Compact(array) => Some(array.values().inner()),
            Self::Wide(array) => Some(array.values().inner()),
            Self::Range { .. } => None,
        }
    }

    fn nulls(&self) -> Option<&arrow::buffer::NullBuffer> {
        match self {
            Self::Compact(array) => array.nulls(),
            Self::Wide(array) => array.nulls(),
            Self::Range { .. } => None,
        }
    }

    fn max(&self) -> Option<u64> {
        match self {
            Self::Compact(array) => arrow::compute::max(array).map(u64::from),
            Self::Wide(array) => arrow::compute::max(array),
            Self::Range { positions, .. } => {
                (!positions.is_empty()).then(|| (positions.end - 1) as u64)
            }
        }
    }

    fn min(&self) -> Option<u64> {
        match self {
            Self::Compact(array) => arrow::compute::min(array).map(u64::from),
            Self::Wide(array) => arrow::compute::min(array),
            Self::Range { positions, .. } => {
                (!positions.is_empty()).then_some(positions.start as u64)
            }
        }
    }

    fn value(&self, row: usize) -> u64 {
        match self {
            Self::Compact(array) => u64::from(array.value(row)),
            Self::Wide(array) => array.value(row),
            Self::Range { positions, .. } => (positions.start + row) as u64,
        }
    }

    fn values(&self, range: std::ops::Range<usize>) -> impl Iterator<Item = u64> + '_ {
        let (compact, wide, positions): (&[u32], &[u64], _) = match self {
            Self::Compact(array) => (&array.values()[range], &[], 0..0),
            Self::Wide(array) => (&[], &array.values()[range], 0..0),
            Self::Range { positions, .. } => (
                &[],
                &[],
                positions.start + range.start..positions.start + range.end,
            ),
        };
        compact
            .iter()
            .copied()
            .map(u64::from)
            .chain(wide.iter().copied())
            .chain(positions.map(|row| row as u64))
    }

    fn slice(&self, offset: usize, length: usize) -> Self {
        match self {
            Self::Compact(array) => Self::Compact(array.slice(offset, length)),
            Self::Wide(array) => Self::Wide(array.slice(offset, length)),
            Self::Range { positions, width } => {
                let start = positions.start + offset;
                width.contiguous_positions(start..start + length)
            }
        }
    }
}

/// Retain DataFusion's streamed index buffers directly. Concatenating a large
/// filtered/sorted result would allocate another full result-sized vector.
/// Prefix ends support random cell lookup; export visits each chunk as a slice.
#[derive(Debug, Default)]
struct RowIndices {
    chunks: Vec<RowIndexChunk>,
    shapes: Vec<IndexChunkShape>,
    ends: Vec<usize>,
    rows: usize,
    bytes: usize,
}

/// Export-only metadata. Computing it lazily keeps ordinary filtering and
/// visible-cell access from traversing an index a second time. Export ranges
/// can then reuse a chunk's bounds and continuity without rescanning its rows.
#[derive(Debug)]
struct IndexChunkShape {
    max: u64,
    details: OnceLock<IndexChunkDetails>,
}

#[derive(Debug)]
struct IndexChunkDetails {
    min: u64,
    contiguous: bool,
}

impl IndexChunkShape {
    fn details(&self, chunk: &RowIndexChunk) -> &IndexChunkDetails {
        self.details.get_or_init(|| {
            let min = chunk.min().expect("nonempty index chunk");
            let first = chunk.value(0);
            let len = chunk.len();
            let contiguous = matches!(chunk, RowIndexChunk::Range { .. })
                || (self.max - min == len as u64 - 1
                    && chunk
                        .values(0..len)
                        .enumerate()
                        .all(|(offset, row)| first.checked_add(offset as u64) == Some(row)));
            IndexChunkDetails { min, contiguous }
        })
    }
}

impl RowIndices {
    fn new(chunks: Vec<RowIndexChunk>, base_rows: usize) -> Result<Self> {
        let mut result = Self::default();
        let mut buffers = std::collections::HashSet::new();
        let mut retained_bytes = |buffer: &arrow::buffer::Buffer| {
            if buffers.insert((buffer.data_ptr().as_ptr() as usize, buffer.capacity())) {
                buffer.capacity()
            } else {
                0
            }
        };
        let index_type = chunks.first().map(RowIndexChunk::data_type);
        for chunk in chunks {
            ensure!(
                Some(chunk.data_type()) == index_type,
                "Mixed row-index widths"
            );
            let nulls = chunk.nulls();
            ensure!(
                nulls.is_none_or(|nulls| nulls.null_count() == 0),
                "A result row index is missing"
            );
            let max = chunk.max();
            ensure!(
                max.is_none_or(|max| max < base_rows as u64),
                "A result row index is outside the imported window"
            );
            let len = chunk.len();
            if len == 0 {
                continue;
            }
            result.rows = result
                .rows
                .checked_add(len)
                .context("Filtered row count overflow")?;
            // A stream may emit several slices of one underlying allocation.
            // Charge each retained buffer once, including an all-valid bitmap;
            // summing Array::get_array_memory_size would charge its capacity
            // repeatedly. This remains Arrow accounting, never process RSS.
            let mut bytes = std::mem::size_of::<RowIndexChunk>()
                .saturating_add(std::mem::size_of::<IndexChunkShape>())
                .saturating_add(chunk.buffer().map_or(0, &mut retained_bytes));
            if let Some(nulls) = nulls {
                bytes = bytes.saturating_add(retained_bytes(nulls.buffer()));
            }
            result.bytes = result.bytes.saturating_add(bytes);
            result.ends.push(result.rows);
            result.shapes.push(IndexChunkShape {
                max: max.expect("nonempty index chunk"),
                details: OnceLock::new(),
            });
            result.chunks.push(chunk);
        }
        Ok(result)
    }

    fn value(&self, row: usize) -> u64 {
        let index = self.ends.partition_point(|&end| end <= row);
        let start = index.checked_sub(1).map_or(0, |index| self.ends[index]);
        self.chunks[index].value(row - start)
    }

    fn chunk_ranges(
        &self,
        range: std::ops::Range<usize>,
    ) -> impl Iterator<Item = (usize, std::ops::Range<usize>)> + '_ {
        let first = self.ends.partition_point(|&end| end <= range.start);
        let last = self.ends.partition_point(|&end| end < range.end);
        let count = if range.is_empty() {
            0
        } else {
            last + 1 - first
        };
        (first..first + count).map(move |index| {
            let start = index.checked_sub(1).map_or(0, |index| self.ends[index]);
            let low = range.start.saturating_sub(start);
            let high = range.end.min(self.ends[index]) - start;
            (index, low..high)
        })
    }

    fn range_values(&self, range: std::ops::Range<usize>) -> impl Iterator<Item = u64> + '_ {
        self.chunk_ranges(range)
            .flat_map(|(index, range)| self.chunks[index].values(range))
    }
}

#[derive(Debug)]
struct TableData {
    columns: Vec<ColumnInfo>,
    schema: arrow::datatypes::SchemaRef,
    batches: Vec<RecordBatch>,
    ends: Vec<usize>,
    rows: usize,
    bytes: usize,
    // Retained logical bytes per column. Shared slices created to align chunks
    // must not charge the same source allocation once per new slice.
    column_bytes: Vec<usize>,
    hints: Vec<Hint>,
    // Original, one-based dataset positions. Kept out of the user schema and
    // retained as typed Arrow buffers rather than per-row strings or Cells.
    source_row_numbers: Vec<UInt64Array>,
    source_row_bytes: usize,
    source_row_start: Option<usize>,
    leases: Vec<crate::resource_budget::MemoryLease>,
}

impl Default for TableData {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            schema: Arc::new(arrow::datatypes::Schema::empty()),
            batches: Vec::new(),
            ends: Vec::new(),
            rows: 0,
            bytes: 0,
            column_bytes: Vec::new(),
            hints: Vec::new(),
            source_row_numbers: Vec::new(),
            source_row_bytes: 0,
            source_row_start: None,
            leases: Vec::new(),
        }
    }
}

impl ArrowTable {
    pub(crate) fn has_row_indices(&self) -> bool {
        self.indices.is_some()
    }

    /// Share the loaded buffers and provenance without applying a view's row order.
    pub(crate) fn without_row_indices(&self) -> Self {
        Self {
            data: self.data.clone(),
            indices: None,
        }
    }

    pub fn new(columns: Vec<ColumnInfo>, batches: Vec<RecordBatch>) -> Result<Self> {
        let schema = batches.first().map_or_else(
            || Arc::new(arrow::datatypes::Schema::empty()),
            RecordBatch::schema,
        );
        Self::new_with_schema(columns, batches, schema)
    }

    /// Keep the reader's schema even when a selected window contains no rows.
    pub(crate) fn new_with_schema(
        columns: Vec<ColumnInfo>,
        batches: Vec<RecordBatch>,
        schema: arrow::datatypes::SchemaRef,
    ) -> Result<Self> {
        ensure!(
            schema.fields().len() == columns.len(),
            "Arrow result schema differs from the snapshot"
        );
        let mut data = TableData {
            hints: columns.iter().map(|c| Hint::parse(&c.data_type)).collect(),
            column_bytes: vec![0; columns.len()],
            ends: Vec::with_capacity(batches.len()),
            columns,
            schema,
            batches,
            ..Default::default()
        };
        for batch in &data.batches {
            ensure!(
                batch.schema_ref() == &data.schema,
                "Arrow result batches have different schemas"
            );
            ensure!(
                batch.num_columns() == data.columns.len(),
                "Arrow result schema differs from the snapshot"
            );
            data.rows = data
                .rows
                .checked_add(batch.num_rows())
                .ok_or_else(|| anyhow::anyhow!("Arrow row count overflow"))?;
            for (bytes, array) in data.column_bytes.iter_mut().zip(batch.columns()) {
                let size = array.get_array_memory_size();
                *bytes = bytes.saturating_add(size);
                data.bytes = data.bytes.saturating_add(size);
            }
            data.ends.push(data.rows);
        }
        Ok(Self {
            data: Arc::new(data),
            indices: None,
        })
    }
    pub fn row_count(&self) -> usize {
        self.indices
            .as_ref()
            .map_or(self.data.rows, |indices| indices.rows)
    }
    pub fn is_empty(&self) -> bool {
        self.row_count() == 0
    }
    pub fn memory_size(&self) -> usize {
        self.data
            .bytes
            .saturating_add(self.data.source_row_bytes)
            .saturating_add(self.indices.as_ref().map_or(0, |indices| indices.bytes))
    }
    pub(crate) fn schema(&self) -> arrow::datatypes::SchemaRef {
        self.data.schema.clone()
    }
    pub(crate) fn raw_batches(&self) -> &[RecordBatch] {
        &self.data.batches
    }

    pub(crate) fn raw_batch_start(&self, index: usize) -> usize {
        index
            .checked_sub(1)
            .map_or(0, |index| self.data.ends[index])
    }

    pub(crate) fn base_row_count(&self) -> usize {
        self.data.rows
    }

    pub(crate) fn base_column_memory_size(&self, column: usize) -> Option<usize> {
        self.data.column_bytes.get(column).copied()
    }

    /// Find a calendar seed in the loaded Arrow data's original row order.
    /// Filtering, sorting, and Top N do not change the underlying batches.
    pub(crate) fn first_calendar_date(
        &self,
        field: &str,
        path: &[String],
    ) -> Option<chrono::NaiveDate> {
        let column = self.data.schema.index_of(field).ok()?;
        self.raw_batches()
            .iter()
            .find_map(|batch| first_calendar_date_in_array(batch.column(column).as_ref(), path))
    }

    /// Select/reorder fields without retaining hidden value arrays or copying
    /// any values. Logical row indices and source positions remain unchanged.
    pub(crate) fn project_columns(&self, indices: &[usize]) -> Result<Self> {
        let mut seen = std::collections::HashSet::with_capacity(indices.len());
        for &index in indices {
            ensure!(
                index < self.data.columns.len(),
                "Arrow projection column is out of range: {index}"
            );
            ensure!(
                seen.insert(index),
                "Arrow projection contains a duplicate column: {index}"
            );
        }
        if indices.len() == self.data.columns.len()
            && indices
                .iter()
                .enumerate()
                .all(|(index, &column)| index == column)
        {
            return Ok(self.clone());
        }
        let column_bytes = indices
            .iter()
            .map(|&index| self.data.column_bytes[index])
            .collect::<Vec<_>>();
        let bytes = column_bytes
            .iter()
            .fold(0usize, |total, bytes| total.saturating_add(*bytes));
        // A field change can remove buffers from several prior reads.
        // Recount this projection instead of retaining every display charge.
        let mut leases = Vec::new();
        let mut has_display_lease = false;
        for lease in &self.data.leases {
            if lease.is_display() {
                has_display_lease = true;
            } else {
                leases.push(lease.clone());
            }
        }
        if has_display_lease {
            leases.push(crate::resource_budget::MemoryLease::display(
                bytes.saturating_add(self.data.source_row_bytes),
            ));
        }
        Ok(Self {
            data: Arc::new(TableData {
                columns: indices
                    .iter()
                    .map(|&index| self.data.columns[index].clone())
                    .collect(),
                schema: Arc::new(self.data.schema.project(indices)?),
                batches: self
                    .data
                    .batches
                    .iter()
                    .map(|batch| batch.project(indices))
                    .collect::<std::result::Result<_, _>>()?,
                ends: self.data.ends.clone(),
                rows: self.data.rows,
                bytes,
                column_bytes,
                hints: indices
                    .iter()
                    .map(|&index| self.data.hints[index].clone())
                    .collect(),
                source_row_numbers: self.data.source_row_numbers.clone(),
                source_row_bytes: self.data.source_row_bytes,
                source_row_start: self.data.source_row_start,
                leases,
            }),
            indices: self.indices.clone(),
        })
    }

    /// Join disjoint columns from two unfiltered windows of the same source.
    /// The caller owns dataset identity; row count and provenance must agree.
    /// Differing chunk boundaries are aligned with shared Arrow slices.
    pub(crate) fn merge_columns(&self, other: &Self) -> Result<Self> {
        ensure!(
            self.indices.is_none() && other.indices.is_none(),
            "Only unfiltered Arrow bases can merge columns"
        );
        ensure!(
            self.data.rows == other.data.rows,
            "Arrow column windows have different row counts"
        );
        ensure!(
            self.data.source_row_start == other.data.source_row_start,
            "Arrow column windows have different source positions"
        );
        let explicit_rows =
            self.data.source_row_start.is_none() && !self.data.source_row_numbers.is_empty();
        ensure!(
            self.data.rows == 0
                || self.data.source_row_start.is_some()
                || explicit_rows != other.data.source_row_numbers.is_empty(),
            "Arrow column windows have different provenance"
        );
        let mut names = std::collections::HashSet::new();
        for column in self.data.columns.iter().chain(&other.data.columns) {
            ensure!(
                names.insert(&column.name),
                "Arrow column windows contain duplicate field {:?}",
                column.name
            );
        }
        let mut metadata = self.data.schema.metadata().clone();
        for (key, value) in other.data.schema.metadata() {
            if let Some(previous) = metadata.get(key) {
                ensure!(
                    previous == value,
                    "Arrow column windows have conflicting Schema metadata: {key}"
                );
            } else {
                metadata.insert(key.clone(), value.clone());
            }
        }
        let schema = Arc::new(arrow::datatypes::Schema::new_with_metadata(
            self.data
                .schema
                .fields()
                .iter()
                .chain(other.data.schema.fields())
                .cloned()
                .collect::<Vec<_>>(),
            metadata,
        ));
        let mut batches = Vec::new();
        let mut ends = Vec::new();
        let mut source_row_numbers = Vec::new();
        let mut position = 0;
        while position < self.data.rows {
            let left_index = self.data.ends.partition_point(|&end| end <= position);
            let right_index = other.data.ends.partition_point(|&end| end <= position);
            let left_start = left_index
                .checked_sub(1)
                .map_or(0, |index| self.data.ends[index]);
            let right_start = right_index
                .checked_sub(1)
                .map_or(0, |index| other.data.ends[index]);
            let end = self.data.ends[left_index].min(other.data.ends[right_index]);
            let count = end - position;
            let left_offset = position - left_start;
            let right_offset = position - right_start;
            let shared_columns = |batch: &RecordBatch, offset: usize| {
                if offset == 0 && count == batch.num_rows() {
                    batch.columns().to_vec()
                } else {
                    batch
                        .columns()
                        .iter()
                        .map(|array| array.slice(offset, count))
                        .collect()
                }
            };
            let mut arrays = shared_columns(&self.data.batches[left_index], left_offset);
            arrays.extend(shared_columns(
                &other.data.batches[right_index],
                right_offset,
            ));
            if explicit_rows {
                let left = self
                    .data
                    .source_row_numbers
                    .get(left_index)
                    .context("Missing Arrow source positions")?
                    .slice(left_offset, count);
                let right = other
                    .data
                    .source_row_numbers
                    .get(right_index)
                    .context("Missing Arrow source positions")?
                    .slice(right_offset, count);
                ensure!(
                    left == right,
                    "Arrow column windows have different source row identities"
                );
                source_row_numbers.push(left);
            }
            batches.push(RecordBatch::try_new_with_options(
                schema.clone(),
                arrays,
                &arrow::record_batch::RecordBatchOptions::new().with_row_count(Some(count)),
            )?);
            ends.push(end);
            position = end;
        }
        Ok(Self {
            data: Arc::new(TableData {
                columns: self
                    .data
                    .columns
                    .iter()
                    .chain(&other.data.columns)
                    .cloned()
                    .collect(),
                schema,
                batches,
                ends,
                rows: self.data.rows,
                bytes: self.data.bytes.saturating_add(other.data.bytes),
                column_bytes: self
                    .data
                    .column_bytes
                    .iter()
                    .chain(&other.data.column_bytes)
                    .copied()
                    .collect(),
                hints: self
                    .data
                    .hints
                    .iter()
                    .chain(&other.data.hints)
                    .cloned()
                    .collect(),
                source_row_numbers,
                source_row_bytes: self.data.source_row_bytes,
                source_row_start: self.data.source_row_start,
                leases: self
                    .data
                    .leases
                    .iter()
                    .chain(&other.data.leases)
                    .cloned()
                    .collect(),
            }),
            indices: None,
        })
    }
    /// The query returns BASE positions, not positions within a previous view.
    /// All value arrays, provenance and lifetime accounting remain shared.
    #[cfg(test)]
    pub(crate) fn with_row_indices(&self, indices: UInt64Array) -> Result<Self> {
        self.with_row_index_batches(vec![indices])
    }

    pub(crate) fn with_row_index_batches(
        &self,
        indices: Vec<impl Into<RowIndexChunk>>,
    ) -> Result<Self> {
        Ok(Self {
            data: self.data.clone(),
            indices: Some(Arc::new(RowIndices::new(
                indices.into_iter().map(Into::into).collect(),
                self.data.rows,
            )?)),
        })
    }

    /// Map positions from a displayed selection back to the shared base.
    /// Output order, duplicate positions and source provenance are preserved.
    pub(crate) fn select_view_rows(
        &self,
        chunks: impl IntoIterator<Item = UInt64Array>,
    ) -> Result<Self> {
        let width = RowIndexWidth::for_rows(self.data.rows);
        let positions = chunks
            .into_iter()
            .map(|selected| {
                ensure!(selected.null_count() == 0, "A selected row is missing");
                ensure!(
                    arrow::compute::max(&selected).is_none_or(|max| max < self.row_count() as u64),
                    "A selected row is outside the displayed result"
                );
                Ok(width.positions(selected.values().iter().map(|&row| {
                    self.base_row(row as usize)
                        .expect("validated displayed row") as u64
                })))
            })
            .collect::<Result<Vec<_>>>()?;
        self.with_row_index_batches(positions)
    }

    fn base_row(&self, row: usize) -> Option<usize> {
        if row >= self.row_count() {
            return None;
        }
        self.indices.as_ref().map_or(Some(row), |indices| {
            usize::try_from(indices.value(row)).ok()
        })
    }

    /// Materialize a bounded export batch in logical view order. The original
    /// buffers remain untouched. Interleave is an Arrow kernel; this adapter
    /// builds only row-address metadata for the requested output range.
    pub(crate) fn materialize_range(&self, range: std::ops::Range<usize>) -> Result<RecordBatch> {
        ensure!(
            range.start <= range.end && range.end <= self.row_count(),
            "Export range is outside the result"
        );
        if range.is_empty() {
            return Ok(RecordBatch::new_empty(self.schema()));
        }
        if self.data.columns.is_empty() {
            return Ok(RecordBatch::try_new_with_options(
                self.schema(),
                Vec::new(),
                &arrow::record_batch::RecordBatchOptions::new().with_row_count(Some(range.len())),
            )?);
        }
        if let Some(batch) = self.materialize_single_batch(range.clone())? {
            return Ok(batch);
        }
        let mut batch_map = std::collections::HashMap::new();
        let mut batches = Vec::new();
        let base_rows = self
            .indices
            .iter()
            .flat_map(|indices| indices.range_values(range.clone()).map(|row| row as usize))
            .chain(
                self.indices
                    .is_none()
                    .then_some(range.clone())
                    .into_iter()
                    .flatten(),
            );
        let rows = base_rows
            .map(|row| {
                let batch = self.data.ends.partition_point(|end| row >= *end);
                let start = if batch == 0 {
                    0
                } else {
                    self.data.ends[batch - 1]
                };
                let local = *batch_map.entry(batch).or_insert_with(|| {
                    let local = batches.len();
                    batches.push(&self.data.batches[batch]);
                    local
                });
                (local, row - start)
            })
            .collect::<Vec<_>>();
        if let Some(&(first_batch, first_row)) = rows.first()
            && rows.iter().enumerate().all(|(offset, &(batch, row))| {
                batch == first_batch && first_row.checked_add(offset) == Some(row)
            })
        {
            // A source-ordered filter commonly leaves long contiguous runs.
            // Slice those values directly; only a sparse or reordered run
            // needs Arrow interleave to allocate new value/offset buffers.
            return Ok(batches[first_batch].slice(first_row, rows.len()));
        }
        Ok(arrow::compute::interleave_record_batch(&batches, &rows)?)
    }

    /// Common filtered exports touch only one imported batch. Keep continuous
    /// values shared, or pass the compact Arrow row index directly to `take`.
    /// General cross-batch/reordered ranges retain the interleave path above.
    fn materialize_single_batch(
        &self,
        range: std::ops::Range<usize>,
    ) -> Result<Option<RecordBatch>> {
        let first = self.base_row(range.start).expect("nonempty valid range");
        let batch_index = self.data.ends.partition_point(|&end| end <= first);
        let batch_start = batch_index
            .checked_sub(1)
            .map_or(0, |index| self.data.ends[index]);
        let batch_end = self.data.ends[batch_index];
        let batch = &self.data.batches[batch_index];
        let Some(indices) = &self.indices else {
            return Ok(
                (range.end <= batch_end).then(|| batch.slice(first - batch_start, range.len()))
            );
        };
        // Arrow 58's List take reserves child capacity from the retained child
        // length divided by the sliced parent length. A small parent slice can
        // therefore allocate as if it selected the whole original child array.
        // Restrict this optional path to flat values with bounded take buffers;
        // encoded/container types keep the established interleave path.
        let bounded_take = batch.columns().iter().all(|array| {
            let kind = array.data_type();
            kind.is_primitive()
                || matches!(
                    kind,
                    DataType::Null
                        | DataType::Boolean
                        | DataType::Utf8
                        | DataType::LargeUtf8
                        | DataType::Utf8View
                        | DataType::Binary
                        | DataType::LargeBinary
                        | DataType::BinaryView
                        | DataType::FixedSizeBinary(_)
                )
        });
        let mut contiguous = true;
        let mut expected = first as u64;
        for (index, local) in indices.chunk_ranges(range.clone()) {
            let chunk = &indices.chunks[index];
            let shape = &indices.shapes[index];
            // The maximum was already needed for index validation. Reject a
            // cross-batch chunk before computing any export-only metadata.
            if shape.max >= batch_end as u64 {
                return Ok(None);
            }
            let details = shape.details(chunk);
            if details.min < batch_start as u64 {
                return Ok(None);
            }
            if !details.contiguous && !bounded_take {
                return Ok(None);
            }
            if !details.contiguous && local.len() != chunk.len() {
                // A smaller part of an otherwise sparse chunk may still be
                // continuous. Preserve the general path's shared-slice check
                // rather than force a new value allocation for that part.
                return Ok(None);
            }
            contiguous &= details.contiguous && chunk.value(local.start) == expected;
            expected = chunk.value(local.end - 1).saturating_add(1);
        }
        if contiguous {
            return Ok(Some(batch.slice(first - batch_start, range.len())));
        }
        if !bounded_take {
            return Ok(None);
        }
        let mut pieces = indices
            .chunk_ranges(range.clone())
            .map(|(index, local)| {
                indices.chunks[index]
                    .slice(local.start, local.len())
                    .into_array()
            })
            .collect::<Vec<_>>();
        let global: ArrayRef = if pieces.len() == 1 {
            pieces.pop().expect("nonempty range")
        } else {
            let arrays = pieces.iter().map(AsRef::as_ref).collect::<Vec<_>>();
            arrow::compute::concat(&arrays)?
        };
        let local = if batch_start == 0 {
            global
        } else {
            use arrow::compute::kernels::numeric::sub;
            match global.data_type() {
                DataType::UInt32 => sub(
                    &global.as_ref(),
                    &UInt32Array::new_scalar(u32::try_from(batch_start)?),
                )?,
                DataType::UInt64 => sub(
                    &global.as_ref(),
                    &UInt64Array::new_scalar(u64::try_from(batch_start)?),
                )?,
                _ => unreachable!("validated row-index type"),
            }
        };
        Ok(Some(arrow::compute::take_record_batch(
            batch,
            local.as_ref(),
        )?))
    }

    /// Emit bounded batches; unfiltered contiguous results use zero-copy slices.
    pub(crate) fn export_batches<'a>(
        &'a self,
        max_rows: usize,
        cancel: &'a std::sync::atomic::AtomicBool,
    ) -> impl Iterator<Item = Result<RecordBatch>> + 'a {
        let mut row = 0usize;
        std::iter::from_fn(move || {
            if row >= self.row_count() {
                return None;
            }
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                row = self.row_count();
                return Some(Err(anyhow::anyhow!("Operation cancelled")));
            }
            let end = row.saturating_add(max_rows.max(1)).min(self.row_count());
            if self.indices.is_none() {
                let batch = self.data.ends.partition_point(|end| row >= *end);
                let start = if batch == 0 {
                    0
                } else {
                    self.data.ends[batch - 1]
                };
                let count = (end - row).min(self.data.ends[batch] - row);
                let output = self.data.batches[batch].slice(row - start, count);
                row += count;
                Some(Ok(output))
            } else {
                let indices = self.indices.as_ref().expect("indexed export");
                let index = indices.ends.partition_point(|&end| end <= row);
                let start = index.checked_sub(1).map_or(0, |i| indices.ends[i]);
                let chunk = &indices.chunks[index];
                if indices.shapes[index].details(chunk).contiguous {
                    let first = chunk.value(row - start) as usize;
                    let batch = self.data.ends.partition_point(|&end| end <= first);
                    let batch_start = batch.checked_sub(1).map_or(0, |i| self.data.ends[i]);
                    let count = (end - row)
                        .min(indices.ends[index] - row)
                        .min(self.data.ends[batch] - first);
                    // Split at imported-batch/index-chunk boundaries instead
                    // of joining continuous values into another owned array.
                    let output = self.data.batches[batch].slice(first - batch_start, count);
                    row += count;
                    return Some(Ok(output));
                }
                let output = self.materialize_range(row..end);
                row = end;
                Some(output)
            }
        })
    }
    pub fn source_row_number(&self, row: usize) -> Option<usize> {
        let row = self.base_row(row)?;
        if let Some(start) = self.data.source_row_start {
            return start.checked_add(row);
        }
        if self.data.source_row_numbers.is_empty() {
            return None;
        }
        let batch = self.data.ends.partition_point(|end| row >= *end);
        let start = if batch == 0 {
            0
        } else {
            self.data.ends[batch - 1]
        };
        let values = self.data.source_row_numbers.get(batch)?;
        (!values.is_null(row - start))
            .then(|| usize::try_from(values.value(row - start)).ok())
            .flatten()
    }
    pub(crate) fn with_source_row_start(mut self, start: usize) -> Self {
        Arc::get_mut(&mut self.data)
            .expect("attach positions before publishing Arrow data")
            .source_row_start = Some(start);
        self
    }

    /// Attach one-based file/partition positions to a newly decoded result.
    /// A source scan can skip rows before decoding the projected output, so
    /// its compact base positions and original positions are independent.
    pub(crate) fn with_source_row_numbers(mut self, numbers: Vec<UInt64Array>) -> Result<Self> {
        let data = Arc::get_mut(&mut self.data)
            .context("Attach positions before publishing Arrow data")?;
        ensure!(
            numbers.len() == data.batches.len()
                && numbers.iter().zip(&data.batches).all(|(rows, batch)| {
                    rows.len() == batch.num_rows() && rows.null_count() == 0
                }),
            "Original row positions do not match the decoded batches"
        );
        data.source_row_bytes = numbers.iter().fold(0usize, |total, rows| {
            total.saturating_add(rows.get_array_memory_size())
        });
        data.source_row_start = None;
        data.source_row_numbers = numbers;
        Ok(self)
    }
    #[cfg(test)]
    pub(crate) fn collect_stream(
        columns: Vec<ColumnInfo>,
        stream: impl Iterator<Item = RecordBatch>,
        resources: &crate::resource_budget::Client,
        cancel: &std::sync::atomic::AtomicBool,
        on_rows: impl FnMut(usize),
    ) -> Result<Self> {
        Self::collect_stream_impl(columns, stream, resources, cancel, on_rows, false)
    }
    #[cfg(test)]
    fn collect_stream_impl(
        columns: Vec<ColumnInfo>,
        mut stream: impl Iterator<Item = RecordBatch>,
        resources: &crate::resource_budget::Client,
        cancel: &std::sync::atomic::AtomicBool,
        mut on_rows: impl FnMut(usize),
        with_source_rows: bool,
    ) -> Result<Self> {
        // Imported data stays in RAM as typed Arrow buffers. No per-row Cells,
        // display strings, or deep-cloned arrays are constructed here.
        let lease = resources.reserve_display(0)?;
        let mut data = TableData {
            hints: columns.iter().map(|c| Hint::parse(&c.data_type)).collect(),
            column_bytes: vec![0; columns.len()],
            columns,
            leases: vec![lease],
            ..Default::default()
        };
        let display_columns = with_source_rows.then(|| (0..data.columns.len()).collect::<Vec<_>>());
        loop {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Acquire),
                "Operation cancelled"
            );
            let Some(batch) = stream.next() else { break };
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Acquire),
                "Operation cancelled"
            );
            data.bytes = data
                .bytes
                .checked_add(batch.get_array_memory_size())
                .ok_or_else(|| anyhow::anyhow!("Arrow byte count overflow"))?;
            data.rows = data
                .rows
                .checked_add(batch.num_rows())
                .ok_or_else(|| anyhow::anyhow!("Arrow row count overflow"))?;
            data.leases[0].grow_to(data.bytes, cancel)?;
            if with_source_rows {
                ensure!(
                    batch.num_columns() == data.columns.len() + 1,
                    "Arrow provenance schema differs from the snapshot"
                );
                let numbers = batch
                    .column(data.columns.len())
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Original row numbers must be unsigned integers")
                    })?;
                ensure!(numbers.null_count() == 0, "Original row number is missing");
                data.source_row_numbers.push(numbers.clone());
                // project only clones ArrayRef handles; payload buffers remain
                // shared, with no second allocation of any data column.
                data.batches
                    .push(batch.project(display_columns.as_deref().unwrap())?);
            } else {
                ensure!(
                    batch.num_columns() == data.columns.len(),
                    "Arrow result schema differs from the snapshot"
                );
                data.batches.push(batch);
            }
            for (bytes, array) in data
                .column_bytes
                .iter_mut()
                .zip(data.batches.last().unwrap().columns())
            {
                *bytes = bytes.saturating_add(array.get_array_memory_size());
            }
            data.ends.push(data.rows);
            on_rows(data.rows);
        }
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "Operation cancelled"
        );
        data.schema = data.batches.first().map_or_else(
            || Arc::new(arrow::datatypes::Schema::empty()),
            RecordBatch::schema,
        );
        Ok(Self {
            data: Arc::new(data),
            indices: None,
        })
    }
    pub(crate) fn retain_memory(&mut self, lease: crate::resource_budget::MemoryLease) {
        Arc::get_mut(&mut self.data)
            .expect("attach lease before publishing Arrow data")
            .leases
            .push(lease);
    }
    /// Resolve a result position through the row index and any dictionary.
    pub(crate) fn array(&self, row: usize, col: usize) -> Option<(&dyn Array, usize)> {
        let row = self.base_row(row)?;
        if col >= self.data.columns.len() {
            return None;
        }
        let batch = self.data.ends.partition_point(|end| row >= *end);
        let start = if batch == 0 {
            0
        } else {
            self.data.ends[batch - 1]
        };
        Some(dictionary_value(
            self.data.batches[batch].column(col).as_ref(),
            row - start,
        ))
    }
    pub fn is_null(&self, row: usize, col: usize) -> bool {
        self.array(row, col).is_none_or(|(a, r)| logical_null(a, r))
    }
    pub fn is_nested(&self, row: usize, col: usize) -> bool {
        self.array(row, col).is_some_and(|(a, r)| {
            !logical_null(a, r)
                && (nested(a.data_type()) || self.data.hints[col].label.starts_with("UNION("))
        })
    }
    pub fn binary(&self, row: usize, col: usize) -> Option<&[u8]> {
        let (array, row) = self.array(row, col)?;
        if logical_null(array, row) {
            return None;
        }
        if let Some(array) = array.as_any().downcast_ref::<StructArray>() {
            return binary(array.column_by_name("bytes")?.as_ref(), row);
        }
        if self.data.columns[col].data_type == "BLOB" {
            binary(array, row)
        } else {
            None
        }
    }
    pub fn display_text(&self, row: usize, col: usize) -> String {
        let Some((array, row)) = self.array(row, col) else {
            return String::new();
        };
        if logical_null(array, row) {
            return "NULL".into();
        }
        if self.data.hints[col].label.starts_with("UNION(") {
            return "Union · 1 item · Open table".into();
        }
        match array.data_type() {
            DataType::List(_) => {
                return format!("List {}", array.as_list::<i32>().value_length(row));
            }
            DataType::LargeList(_) => {
                return format!("List {}", array.as_list::<i64>().value_length(row));
            }
            DataType::ListView(_) => {
                return format!("List {}", array.as_list_view::<i32>().value_size(row));
            }
            DataType::LargeListView(_) => {
                return format!("List {}", array.as_list_view::<i64>().value_size(row));
            }
            DataType::FixedSizeList(_, count) => return format!("List {count}"),
            DataType::Struct(fields) => return format!("Struct {}", fields.len()),
            DataType::Map(_, _) => {
                let count = array.as_map().value_length(row);
                return format!("Map · {count} entries · Open table");
            }
            DataType::Union(_, _) => return "Union · 1 items · Open table".into(),
            _ => {}
        }
        if self.data.columns[col].data_type == "BLOB" {
            return format!(
                "<binary: {} bytes>",
                binary(array, row).map_or(0, <[u8]>::len)
            );
        }
        if let Some(text) = utf8_value(array, row) {
            let end = text.char_indices().nth(2000).map_or(text.len(), |(i, _)| i);
            return text[..end].to_owned();
        }
        scalar(array, row, &self.data.hints[col].label)
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        let Some((array, index)) = self.array(row, col) else {
            return Cell {
                is_null: true,
                text: "NULL".into(),
                ..Default::default()
            };
        };
        if logical_null(array, index) {
            return Cell {
                is_null: true,
                text: "NULL".into(),
                ..Default::default()
            };
        }
        let json = (nested(array.data_type()) || self.data.hints[col].label.starts_with("UNION("))
            .then(|| json_value(array, index, &self.data.hints[col]));
        let text = json.as_ref().map_or_else(
            || {
                if self.data.columns[col].data_type == "BLOB" {
                    self.display_text(row, col)
                } else {
                    scalar(array, index, &self.data.hints[col].label)
                }
            },
            |value| serde_json::to_string(value).unwrap_or_default(),
        );
        Cell {
            text,
            is_null: false,
            bytes: self.binary(row, col).map(<[u8]>::to_vec),
            json,
        }
    }
    /// Retain one column in one batch for a deferred explicit action. Arrow
    /// slicing shares its buffers; other columns/batches and TableData are not
    /// retained. The existing lease follows the shared allocation's lifetime.
    pub fn slice_cell(&self, row: usize, col: usize) -> Option<Self> {
        let (array, index) = self.array(row, col)?;
        let field = self
            .data
            .schema
            .field(col)
            .clone()
            .with_data_type(array.data_type().clone());
        let schema = Arc::new(arrow::datatypes::Schema::new(vec![field]));
        let batch = RecordBatch::try_new(schema, vec![array.slice(index, 1)]).ok()?;
        let mut data = Self::new(vec![self.data.columns[col].clone()], vec![batch]).ok()?;
        if let Some(number) = self.source_row_number(row) {
            data = data.with_source_row_start(number);
        }
        for lease in &self.data.leases {
            data.retain_memory(lease.clone());
        }
        Some(data)
    }

    /// Detail payloads are materialized only when opening a Peek. Image mode
    /// avoids constructing a JSON tree; nested Inspect avoids a second JSON
    /// string that Copy Node / Save Value can serialize on explicit demand.
    pub fn preview_cell(&self, row: usize, col: usize, image_only: bool) -> Cell {
        if image_only {
            return Cell {
                text: self.display_text(row, col),
                is_null: self.is_null(row, col),
                bytes: self.binary(row, col).map(<[u8]>::to_vec),
                json: None,
            };
        }
        if !self.is_nested(row, col) {
            return self.cell(row, col);
        }
        let (array, index) = self.array(row, col).expect("validated nested index");
        Cell {
            text: String::new(),
            is_null: false,
            bytes: self.binary(row, col).map(<[u8]>::to_vec),
            json: Some(json_value(array, index, &self.data.hints[col])),
        }
    }
    #[cfg(test)]
    pub fn from_cells(columns: Vec<ColumnInfo>, rows: Vec<Vec<Cell>>) -> Self {
        crate::test_support::table_from_cells(columns, rows).expect("typed Arrow fixture")
    }
}

fn logical_null(array: &dyn Array, row: usize) -> bool {
    let (array, row) = dictionary_value(array, row);
    array.is_null(row)
}
/// Follow only the requested dictionary key, rather than unpacking its whole
/// batch or allocating a logical-null bitmap on every visible-cell access.
fn dictionary_value(array: &dyn Array, row: usize) -> (&dyn Array, usize) {
    arrow::array::downcast_dictionary_array! {
        array => {
            array.key(row).map_or((array as &dyn Array, row), |key| dictionary_value(array.values().as_ref(), key))
        }
        _ => (array, row),
    }
}

fn first_calendar_date_in_array(
    mut array: &dyn Array,
    mut path: &[String],
) -> Option<chrono::NaiveDate> {
    let mut parent_nulls = Vec::new();
    while let Some((name, rest)) = path.split_first() {
        if array.null_count() == array.len() {
            return None;
        }
        if matches!(array.data_type(), DataType::Dictionary(_, _)) {
            // Its children are indexed by dictionary key, not by source row.
            break;
        }
        let structure = array.as_any().downcast_ref::<StructArray>()?;
        if let Some(nulls) = structure.nulls() {
            parent_nulls.push(nulls);
        }
        array = structure.column_by_name(name)?.as_ref();
        path = rest;
    }
    if array.null_count() == array.len() {
        return None;
    }
    let date = |row| {
        if parent_nulls.iter().any(|nulls| nulls.is_null(row)) {
            return None;
        }
        calendar_date_at(array, row, path)
    };
    // Skip runs of missing rows using the most selective available bitmap.
    // Test the other bitmaps in place, without merging them into a new buffer.
    match array
        .nulls()
        .into_iter()
        .chain(parent_nulls.iter().copied())
        .max_by_key(|nulls| nulls.null_count())
    {
        Some(nulls) => nulls.valid_indices().find_map(date),
        None => (0..array.len()).find_map(date),
    }
}

fn calendar_date_at(array: &dyn Array, row: usize, path: &[String]) -> Option<chrono::NaiveDate> {
    let (array, row) = dictionary_value(array, row);
    if array.is_null(row) {
        return None;
    }
    if let Some((name, rest)) = path.split_first() {
        let structure = array.as_any().downcast_ref::<StructArray>()?;
        return calendar_date_at(structure.column_by_name(name)?.as_ref(), row, rest);
    }
    if !matches!(
        array.data_type(),
        DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _)
    ) {
        return None;
    }
    // Use the UTC date of a zoned timestamp, as the table display does.
    arrow::array::downcast_temporal_array!(array => {
        let value = i64::from(array.value(row));
        if matches!(value, i64::MIN | i64::MAX) {
            None
        } else {
            array.value_as_date(row)
        }
    } _ => None)
}

fn utf8_value(array: &dyn Array, row: usize) -> Option<&str> {
    let array = array.as_any();
    array
        .downcast_ref::<StringArray>()
        .map(|a| a.value(row))
        .or_else(|| {
            array
                .downcast_ref::<LargeStringArray>()
                .map(|a| a.value(row))
        })
        .or_else(|| {
            array
                .downcast_ref::<StringViewArray>()
                .map(|a| a.value(row))
        })
}

fn nested(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::List(_)
            | DataType::LargeList(_)
            | DataType::ListView(_)
            | DataType::LargeListView(_)
            | DataType::FixedSizeList(_, _)
            | DataType::Struct(_)
            | DataType::Map(_, _)
            | DataType::Union(_, _)
    )
}
fn binary(array: &dyn Array, row: usize) -> Option<&[u8]> {
    let (array, row) = dictionary_value(array, row);
    if row >= array.len() || array.is_null(row) {
        return None;
    }
    if let Some(a) = array.as_any().downcast_ref::<BinaryArray>() {
        Some(a.value(row))
    } else if let Some(a) = array.as_any().downcast_ref::<LargeBinaryArray>() {
        Some(a.value(row))
    } else if let Some(a) = array.as_any().downcast_ref::<FixedSizeBinaryArray>() {
        Some(a.value(row))
    } else if let Some(a) = array.as_any().downcast_ref::<BinaryViewArray>() {
        Some(a.value(row))
    } else {
        None
    }
}
fn scalar(array: &dyn Array, row: usize, hint: &str) -> String {
    let (array, row) = dictionary_value(array, row);
    if logical_null(array, row) {
        return "NULL".into();
    }
    if hint == "INTERVAL" {
        if let Some(text) = crate::parquet_compat::interval_text(array, row) {
            return text;
        }
    }
    if hint == "BOOLEAN" {
        if let Some(array) = array.as_any().downcast_ref::<Int8Array>() {
            return (array.value(row) != 0).to_string();
        }
    }
    if matches!(hint, "HUGEINT" | "UHUGEINT") {
        if let Some(bytes) = binary(array, row).filter(|bytes| bytes.len() == 16) {
            let raw = <[u8; 16]>::try_from(bytes).unwrap();
            return if hint == "HUGEINT" {
                i128::from_le_bytes(raw).to_string()
            } else {
                u128::from_le_bytes(raw).to_string()
            };
        }
        if hint == "HUGEINT" {
            if let Some(a) = array.as_any().downcast_ref::<Decimal128Array>() {
                return a.value(row).to_string();
            }
        }
        if let Some(a) = array.as_any().downcast_ref::<Decimal128Array>() {
            return (a.value(row) as u128).to_string();
        }
    }
    if matches!(hint, "TIME WITH TIME ZONE" | "TIMETZ") {
        if let Some(bytes) = binary(array, row).filter(|bytes| bytes.len() == 8) {
            let bits = u64::from_le_bytes(bytes.try_into().unwrap());
            let micros = (bits >> 24) as i64;
            let offset = 57599 - (bits & 0xffffff) as i32;
            let seconds = offset.unsigned_abs();
            let mut text = format!(
                "{}{}{:02}",
                clock(micros / 1_000_000, (micros % 1_000_000) * 1000),
                if offset < 0 { '-' } else { '+' },
                seconds / 3600
            );
            if seconds % 3600 != 0 {
                text.push_str(&format!(":{:02}", seconds / 60 % 60));
            }
            if seconds % 60 != 0 {
                text.push_str(&format!(":{:02}", seconds % 60));
            }
            return text;
        }
    }
    if hint == "BIT" {
        if let Some(bytes) = binary(array, row).filter(|bytes| bytes.len() >= 2) {
            return bytes[1..]
                .iter()
                .enumerate()
                .flat_map(|(i, byte)| {
                    (if i == 0 { usize::from(bytes[0]) } else { 0 }..8).map(move |bit| {
                        if byte & (1 << (7 - bit)) == 0 {
                            '0'
                        } else {
                            '1'
                        }
                    })
                })
                .collect();
        }
    }
    if hint == "BIGNUM" {
        if let Some(bytes) = binary(array, row).filter(|bytes| bytes.len() >= 4) {
            let negative = bytes[0] & 0x80 == 0;
            // Native BIGNUM is a three-byte sign/length header and big-endian
            // magnitude (ones-complement for negatives). Base-1e9 avoids f64.
            let mut digits = vec![0u32];
            for byte in &bytes[3..] {
                let mut carry = u64::from(if negative { !byte } else { *byte });
                for digit in &mut digits {
                    let value = u64::from(*digit) * 256 + carry;
                    *digit = (value % 1_000_000_000) as u32;
                    carry = value / 1_000_000_000;
                }
                if carry != 0 {
                    digits.push(carry as u32);
                }
            }
            let mut text = if negative {
                "-".to_owned()
            } else {
                String::new()
            };
            text.push_str(&digits.pop().unwrap().to_string());
            for digit in digits.iter().rev() {
                text.push_str(&format!("{digit:09}"));
            }
            return text;
        }
    }
    if hint.eq_ignore_ascii_case("UUID") {
        if let Some(bytes) = binary(array, row).filter(|bytes| bytes.len() == 16) {
            let h = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            return format!(
                "{}-{}-{}-{}-{}",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            );
        }
    }
    if let Some(a) = array.as_any().downcast_ref::<Date32Array>() {
        if a.value(row) == i32::MAX {
            return "infinity".into();
        }
        if a.value(row) == i32::MIN {
            return "-infinity".into();
        }
        return date_days(i64::from(a.value(row)));
    }
    let timestamp = match array.data_type() {
        DataType::Timestamp(TimeUnit::Second, _) => array
            .as_any()
            .downcast_ref::<TimestampSecondArray>()
            .map(|a| a.value(row)),
        DataType::Timestamp(TimeUnit::Millisecond, _) => array
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .map(|a| a.value(row)),
        DataType::Timestamp(TimeUnit::Microsecond, _) => array
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .map(|a| a.value(row)),
        DataType::Timestamp(TimeUnit::Nanosecond, _) => array
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .map(|a| a.value(row)),
        _ => None,
    };
    if timestamp == Some(i64::MAX) {
        return "infinity".into();
    }
    if timestamp == Some(i64::MIN) {
        return "-infinity".into();
    }
    if let (Some(value), DataType::Timestamp(unit, zone)) = (timestamp, array.data_type()) {
        let factor = match unit {
            TimeUnit::Second => 1,
            TimeUnit::Millisecond => 1000,
            TimeUnit::Microsecond => 1_000_000,
            TimeUnit::Nanosecond => 1_000_000_000,
        };
        let seconds = value.div_euclid(factor);
        let nanos = value.rem_euclid(factor) * (1_000_000_000 / factor);
        return format!(
            "{} {}{}",
            date_days(seconds.div_euclid(86400)),
            clock(seconds.rem_euclid(86400), nanos),
            if zone.is_some() { "+00" } else { "" }
        );
    }
    if let Some(array) = array.as_any().downcast_ref::<Time64MicrosecondArray>() {
        let value = array.value(row);
        return clock(value / 1_000_000, (value % 1_000_000) * 1000);
    }
    if let Some(array) = array.as_any().downcast_ref::<Time64NanosecondArray>() {
        let value = array.value(row);
        return clock(value / 1_000_000_000, value % 1_000_000_000);
    }
    if let Some(array) = array.as_any().downcast_ref::<IntervalMonthDayNanoArray>() {
        let value = array.value(row);
        let mut parts = Vec::new();
        for (count, unit) in [
            (value.months / 12, "year"),
            (value.months % 12, "month"),
            (value.days, "day"),
        ] {
            if count != 0 {
                parts.push(format!(
                    "{count} {unit}{}",
                    if count.abs() == 1 { "" } else { "s" }
                ));
            }
        }
        if value.nanoseconds != 0 || parts.is_empty() {
            let nanos = value.nanoseconds.unsigned_abs();
            parts.push(format!(
                "{}{}",
                if value.nanoseconds < 0 { "-" } else { "" },
                clock(
                    (nanos / 1_000_000_000) as i64,
                    (nanos % 1_000_000_000) as i64
                )
            ));
        }
        return parts.join(" ");
    }
    let options = FormatOptions::new()
        .with_timestamp_format(Some("%Y-%m-%d %H:%M:%S%.f"))
        .with_timestamp_tz_format(Some("%Y-%m-%d %H:%M:%S%.f%:z"));
    let mut text = ArrayFormatter::try_new(array, &options)
        .map(|f| f.value(row).to_string())
        .unwrap_or_else(|_| {
            array_value_to_string(array, row).unwrap_or_else(|_| "〈unsupported value〉".into())
        });
    if matches!(array.data_type(), DataType::Timestamp(_, Some(_))) && text.ends_with(":00") {
        text.truncate(text.len() - 3);
    }
    text
}

fn date_days(days: i64) -> String {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if year <= 0 {
        format!("{:04}-{month:02}-{day:02} (BC)", 1 - year)
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    }
}
fn clock(seconds: i64, nanos: i64) -> String {
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    );
    if nanos != 0 {
        text.push('.');
        text.push_str(format!("{nanos:09}").trim_end_matches('0'));
    }
    text
}

#[derive(Clone, Debug, Default)]
struct Hint {
    label: String,
    fields: Vec<(String, Hint)>,
    child: Option<Box<Hint>>,
    key: Option<Box<Hint>>,
}
impl Hint {
    fn parse(source: &str) -> Self {
        let source = source.trim();
        let mut hint = Self {
            label: source.into(),
            ..Default::default()
        };
        if source.ends_with(']') {
            if let Some(start) = source.rfind('[') {
                hint.child = Some(Box::new(Self::parse(&source[..start])));
                return hint;
            }
        }
        if let Some(body) = source
            .strip_prefix("STRUCT(")
            .or_else(|| source.strip_prefix("UNION("))
            .and_then(|v| v.strip_suffix(')'))
        {
            for field in split_types(body) {
                if let Some((name, kind)) = field_name(field) {
                    hint.fields.push((name, Self::parse(kind)));
                }
            }
        } else if let Some(body) = source
            .strip_prefix("MAP(")
            .and_then(|v| v.strip_suffix(')'))
        {
            let fields = split_types(body);
            if fields.len() == 2 {
                hint.key = Some(Box::new(Self::parse(fields[0])));
                hint.child = Some(Box::new(Self::parse(fields[1])));
            }
        } else if let Some(body) = source
            .strip_prefix("LIST(")
            .and_then(|v| v.strip_suffix(')'))
        {
            hint.child = Some(Box::new(Self::parse(body)));
        }
        hint
    }
    fn field(&self, name: &str) -> &Self {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map_or(self, |(_, h)| h)
    }
}
fn split_types(source: &str) -> Vec<&str> {
    let (mut depth, mut quoted, mut start) = (0usize, false, 0usize);
    let mut fields = Vec::new();
    for (index, ch) in source.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '(' | '[' if !quoted => depth += 1,
            ')' | ']' if !quoted => depth = depth.saturating_sub(1),
            ',' if !quoted && depth == 0 => {
                fields.push(source[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    fields.push(source[start..].trim());
    fields
}
fn field_name(source: &str) -> Option<(String, &str)> {
    if let Some(rest) = source.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, ch)) = chars.next() {
            if ch == '"' {
                if chars.peek().is_some_and(|(_, c)| *c == '"') {
                    chars.next();
                    name.push('"');
                } else {
                    return Some((name, rest[i + 1..].trim()));
                }
            } else {
                name.push(ch);
            }
        }
        None
    } else {
        let i = source.find(char::is_whitespace)?;
        Some((source[..i].into(), source[i..].trim()))
    }
}
fn json_value(array: &dyn Array, row: usize, hint: &Hint) -> Value {
    json_value_mode(array, row, hint, false)
}
fn json_value_mode(
    array: &dyn Array,
    row: usize,
    hint: &Hint,
    preserve_map_entries: bool,
) -> Value {
    let json_value = |array: &dyn Array, row: usize, hint: &Hint| {
        json_value_mode(array, row, hint, preserve_map_entries)
    };
    let (array, row) = dictionary_value(array, row);
    if logical_null(array, row) {
        return Value::Null;
    }
    if hint.label == "INTERVAL" {
        if let Some(value) = crate::parquet_compat::interval_json(array, row) {
            return value;
        }
    }
    if hint.label == "JSON" {
        let text = scalar(array, row, "VARCHAR");
        return serde_json::from_str(&text).unwrap_or(Value::String(text));
    }
    // Legacy Arrow UNION has no parent validity: SQL NULL and a tagged NULL
    // become identical. Only UNION leaves are exported as exact JSON strings;
    // parent arrays, other children and BLOB buffers remain genuinely typed.
    if hint.label.starts_with("UNION(") {
        if let Some(array) = array.as_any().downcast_ref::<StringArray>() {
            return serde_json::from_str(array.value(row))
                // Legacy encoded cells can contain non-standard tokens (NaN/Infinity).
                // Preserve that entire tagged value as text rather than panic
                // or silently turn it into null; ordinary JSON remains a tree.
                .unwrap_or_else(|_| Value::String(array.value(row).to_owned()));
        }
    }
    let unknown = Hint::default();
    if let Some(a) = array.as_any().downcast_ref::<StructArray>() {
        return Value::Object(
            a.fields()
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    (
                        field.name().clone(),
                        json_value(a.column(i).as_ref(), row, hint.field(field.name())),
                    )
                })
                .collect(),
        );
    }
    let list = if let Some(a) = array.as_any().downcast_ref::<ListArray>() {
        Some(a.value(row))
    } else if let Some(a) = array.as_any().downcast_ref::<LargeListArray>() {
        Some(a.value(row))
    } else if let Some(a) = array.as_any().downcast_ref::<ListViewArray>() {
        Some(a.value(row))
    } else if let Some(a) = array.as_any().downcast_ref::<LargeListViewArray>() {
        Some(a.value(row))
    } else {
        array
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .map(|a| a.value(row))
    };
    if let Some(list) = list {
        return Value::Array(
            (0..list.len())
                .map(|i| json_value(list.as_ref(), i, hint.child.as_deref().unwrap_or(&unknown)))
                .collect(),
        );
    }
    if let Some(a) = array.as_any().downcast_ref::<MapArray>() {
        let entries = a.value(row);
        let object = (0..entries.len())
            .map(|i| {
                (
                    entries
                        .column(0)
                        .as_any()
                        .downcast_ref::<StructArray>()
                        .and_then(|keys| keys.column_by_name("__pv_arrow_text"))
                        .map_or_else(
                            || {
                                let key = entries.column(0).as_ref();
                                let key_hint = hint.key.as_deref().unwrap_or(&unknown);
                                if nested(key.data_type()) {
                                    json_value(key, i, key_hint).to_string()
                                } else {
                                    scalar(key, i, &key_hint.label)
                                }
                            },
                            |text| scalar(text.as_ref(), i, "VARCHAR"),
                        ),
                    json_value(
                        entries.column(1).as_ref(),
                        i,
                        hint.child.as_deref().unwrap_or(&unknown),
                    ),
                )
            })
            .collect::<serde_json::Map<String, Value>>();
        if preserve_map_entries && object.len() != entries.len() {
            // JSON objects cannot preserve duplicate names. Only this rare case
            // uses typed entries, keeping every original Map key and value.
            return Value::Array((0..entries.len()).map(|i| serde_json::json!({
                "key": json_value(entries.column(0).as_ref(), i, hint.key.as_deref().unwrap_or(&unknown)),
                "value": json_value(entries.column(1).as_ref(), i, hint.child.as_deref().unwrap_or(&unknown)),
            })).collect());
        }
        return Value::Object(object);
    }
    if let Some(a) = array.as_any().downcast_ref::<UnionArray>() {
        let id = a.type_id(row);
        let offset = a.value_offset(row);
        let h = if let DataType::Union(fields, _) = a.data_type() {
            fields
                .iter()
                .find(|(t, _)| *t == id)
                .map(|(_, field)| hint.field(field.name()))
                .unwrap_or(hint)
        } else {
            hint
        };
        let name = if let DataType::Union(fields, _) = a.data_type() {
            fields
                .iter()
                .find(|(t, _)| *t == id)
                .map(|(_, f)| f.name().clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        return Value::Object(
            [(name, json_value(a.child(id).as_ref(), offset, h))]
                .into_iter()
                .collect(),
        );
    }
    if hint.label == "BLOB" || hint.label.is_empty() {
        if let Some(bytes) = binary(array, row) {
            let mut text = String::new();
            for &byte in bytes {
                if (32..=126).contains(&byte) && !matches!(byte, b'\\' | b'\'' | b'"') {
                    text.push(byte as char);
                } else {
                    text.push_str(&format!("\\x{byte:02X}"));
                }
            }
            return Value::String(text);
        }
    }
    let text = scalar(array, row, &hint.label);
    if hint.label == "BOOLEAN" || matches!(array.data_type(), DataType::Boolean) {
        return Value::Bool(text == "true");
    }
    if matches!(hint.label.as_str(), "HUGEINT" | "UHUGEINT" | "BIGNUM")
        || matches!(
            array.data_type(),
            DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Float16
                | DataType::Float32
                | DataType::Float64
                | DataType::Decimal32(_, _)
                | DataType::Decimal64(_, _)
                | DataType::Decimal128(_, _)
                | DataType::Decimal256(_, _)
        )
    {
        return text
            .parse::<serde_json::Number>()
            .map(Value::Number)
            .unwrap_or(Value::String(text));
    }
    Value::String(text)
}

/// Prepare logical type hints once per exported column, rather than parsing
/// the same nested type for every scalar written by an I/O format writer.
pub(crate) struct ExportFormatter(Hint);

impl ExportFormatter {
    pub(crate) fn new(data_type: &str) -> Self {
        Self(Hint::parse(data_type))
    }
    pub(crate) fn json_value(&self, array: &dyn Array, row: usize) -> Value {
        json_value_mode(array, row, &self.0, true)
    }
}

#[cfg(test)]
pub(crate) fn fixture_schema(columns: &[ColumnInfo]) -> Result<arrow::datatypes::SchemaRef> {
    use arrow::datatypes::{Field, Schema};
    fn kind(hint: &Hint) -> Result<DataType> {
        if !hint.fields.is_empty() && hint.label.starts_with("STRUCT(") {
            return Ok(DataType::Struct(
                hint.fields
                    .iter()
                    .map(|(name, hint)| Ok(Arc::new(Field::new(name, kind(hint)?, true))))
                    .collect::<Result<Vec<_>>>()?
                    .into(),
            ));
        }
        if let (Some(key), Some(value)) = (&hint.key, &hint.child) {
            return Ok(DataType::Map(
                Arc::new(Field::new(
                    "entries",
                    DataType::Struct(
                        vec![
                            Arc::new(Field::new("key", kind(key)?, false)),
                            Arc::new(Field::new("value", kind(value)?, true)),
                        ]
                        .into(),
                    ),
                    false,
                )),
                false,
            ));
        }
        if let Some(child) = &hint.child {
            let field = Arc::new(Field::new("item", kind(child)?, true));
            let count = hint
                .label
                .rsplit_once('[')
                .and_then(|(_, size)| size.strip_suffix(']'))
                .and_then(|size| size.parse::<i32>().ok());
            return Ok(count.map_or_else(
                || DataType::List(field.clone()),
                |size| DataType::FixedSizeList(field.clone(), size),
            ));
        }
        let upper = hint.label.to_ascii_uppercase();
        if let Some(args) = upper
            .strip_prefix("DECIMAL(")
            .or_else(|| upper.strip_prefix("NUMERIC("))
            .and_then(|args| args.strip_suffix(')'))
        {
            let (precision, scale) = args.split_once(',').context("Invalid fixture Decimal")?;
            let precision = precision.trim().parse::<u8>()?;
            let scale = scale.trim().parse::<i8>()?;
            return Ok(if precision <= 38 {
                DataType::Decimal128(precision, scale)
            } else {
                DataType::Decimal256(precision, scale)
            });
        }
        Ok(match upper.as_str() {
            "TINYINT" | "INT8" => DataType::Int8,
            "SMALLINT" | "INT16" => DataType::Int16,
            "INTEGER" | "INT32" => DataType::Int32,
            "BIGINT" | "INT64" => DataType::Int64,
            "UTINYINT" | "UINT8" => DataType::UInt8,
            "USMALLINT" | "UINT16" => DataType::UInt16,
            "UINTEGER" | "UINT32" => DataType::UInt32,
            "UBIGINT" | "UINT64" => DataType::UInt64,
            "HUGEINT" | "UHUGEINT" => DataType::Decimal256(39, 0),
            "BIGNUM" => DataType::Decimal256(76, 0),
            "FLOAT" | "REAL" => DataType::Float32,
            "DOUBLE" => DataType::Float64,
            "BOOLEAN" | "BOOL" => DataType::Boolean,
            "BLOB" | "BINARY" => DataType::Binary,
            "DATE" => DataType::Date32,
            "TIME" => DataType::Time64(TimeUnit::Microsecond),
            "TIME_NS" => DataType::Time64(TimeUnit::Nanosecond),
            "TIMESTAMP_S" => DataType::Timestamp(TimeUnit::Second, None),
            "TIMESTAMP_MS" => DataType::Timestamp(TimeUnit::Millisecond, None),
            "TIMESTAMP" | "TIMESTAMP_US" => DataType::Timestamp(TimeUnit::Microsecond, None),
            "TIMESTAMP_NS" => DataType::Timestamp(TimeUnit::Nanosecond, None),
            "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => {
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
            }
            _ => DataType::Utf8,
        })
    }
    Ok(Arc::new(Schema::new(
        columns
            .iter()
            .map(|column| {
                Ok(Field::new(
                    &column.name,
                    kind(&Hint::parse(&column.data_type))?,
                    column.nullable,
                ))
            })
            .collect::<Result<Vec<_>>>()?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_cancellation_releases_owned_batches_and_reports_real_rows() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let values = Arc::new(Int64Array::from(vec![10, 20]));
        let observer = Arc::downgrade(&values);
        let batch = RecordBatch::try_from_iter([("id", values.clone() as ArrayRef)]).unwrap();
        drop(values);
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        let mut first = Some(batch);
        let stream = std::iter::from_fn(move || {
            Some(
                first
                    .take()
                    .expect("must not fetch another batch after cancellation"),
            )
        });
        let result = ArrowTable::collect_stream(
            vec![ColumnInfo {
                name: "id".into(),
                data_type: "BIGINT".into(),
                nullable: false,
            }],
            stream,
            &crate::resource_budget::Client::new(),
            &cancel,
            |rows| {
                seen.push(rows);
                cancel.store(true, Ordering::Release);
            },
        );
        assert!(result.unwrap_err().to_string().contains("cancel"));
        assert_eq!(seen, [2]);
        assert!(
            observer.upgrade().is_none(),
            "canceled conversion must release its batches"
        );
    }

    #[test]
    fn streaming_resident_batches_keep_shared_payload_and_slice_lifetime() {
        let binary = BinaryArray::from_vec(vec![&[0, 1, 2, 3][..]]);
        let original = binary.value(0).as_ptr();
        let batch = RecordBatch::try_from_iter([("image", Arc::new(binary) as ArrayRef)]).unwrap();
        let table = ArrowTable::collect_stream(
            vec![ColumnInfo {
                name: "image".into(),
                data_type: "BLOB".into(),
                nullable: true,
            }],
            std::iter::once(batch),
            &crate::resource_budget::Client::new(),
            &std::sync::atomic::AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            table.binary(0, 0).unwrap().as_ptr(),
            original,
            "import must not copy typed buffers"
        );
        let peek = table.slice_cell(0, 0).unwrap();
        assert!(!table.data.leases.is_empty());
        assert!(!peek.data.leases.is_empty());
        drop(table);
        assert_eq!(peek.binary(0, 0), Some(&[0, 1, 2, 3][..]));
        assert_eq!(peek.binary(0, 0).unwrap().as_ptr(), original);
    }

    fn column(name: &str, kind: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: kind.into(),
            nullable: true,
        }
    }

    #[test]
    fn typed_arrow_scalar_precision_dates_and_nulls_remain_exact() {
        use arrow::datatypes::Schema;
        let columns = vec![
            column("big", "BIGINT"),
            column("amount", "DECIMAL(38,18)"),
            column("day", "DATE"),
            column("stamp", "TIMESTAMP_NS"),
            column("flag", "BOOLEAN"),
        ];
        let values: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![9007199254740993])),
            Arc::new(
                Decimal128Array::from(vec![
                    "12345678901234567890123456789012345678"
                        .parse::<i128>()
                        .unwrap(),
                ])
                .with_precision_and_scale(38, 18)
                .unwrap(),
            ),
            Arc::new(Date32Array::from(vec![20090])),
            Arc::new(TimestampNanosecondArray::from(vec![
                chrono::NaiveDateTime::parse_from_str(
                    "2025-01-02 12:30:01.123456789",
                    "%Y-%m-%d %H:%M:%S%.f",
                )
                .unwrap()
                .and_utc()
                .timestamp_nanos_opt()
                .unwrap(),
            ])),
            Arc::new(BooleanArray::from(vec![None])),
        ];
        let fields = columns
            .iter()
            .zip(&values)
            .map(|(column, values)| {
                arrow::datatypes::Field::new(&column.name, values.data_type().clone(), true)
            })
            .collect::<Vec<_>>();
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), values).unwrap();
        let table = ArrowTable::new(columns, vec![batch]).unwrap();
        assert_eq!(table.cell(0, 0).text, "9007199254740993");
        assert_eq!(
            table.cell(0, 1).text,
            "12345678901234567890.123456789012345678"
        );
        assert_eq!(table.cell(0, 2).text, "2025-01-02");
        assert_eq!(table.cell(0, 3).text, "2025-01-02 12:30:01.123456789");
        assert!(table.is_null(0, 4));
        assert_eq!(
            ExportFormatter::new("DECIMAL(38,18)")
                .json_value(table.raw_batches()[0].column(1).as_ref(), 0)
                .to_string(),
            "12345678901234567890.123456789012345678"
        );
    }

    #[test]
    fn calendar_seed_uses_loaded_source_order_and_nested_validity() -> Result<()> {
        use arrow::{buffer::NullBuffer, datatypes::Field};
        use chrono::NaiveDate;

        let make_batch = |dates: Vec<Option<i32>>, parent_valid, stamps| {
            let children = vec![Field::new("event.day", DataType::Date32, true)].into();
            let details = StructArray::new(
                children,
                vec![Arc::new(Date32Array::from(dates))],
                Some(NullBuffer::from(parent_valid)),
            );
            let stamps = TimestampNanosecondArray::from(stamps).with_timezone("+08:00");
            RecordBatch::try_from_iter([
                ("details", Arc::new(details) as ArrayRef),
                ("stamp", Arc::new(stamps) as ArrayRef),
            ])
        };
        let first = make_batch(
            vec![Some(0), Some(1)],
            vec![false, false],
            vec![None, Some(i64::MIN)],
        )?;
        let second = make_batch(
            vec![Some(3), None, Some(i32::MAX), Some(2), Some(-5)],
            vec![false, true, true, true, true],
            vec![Some(-1), Some(0), None, None, None],
        )?;
        let columns = crate::arrow_types::columns_from_schema(first.schema_ref());
        let table = ArrowTable::new(columns, vec![first, second])?
            .with_row_indices(UInt64Array::from(vec![6, 4]))?;

        assert_eq!(
            table.first_calendar_date("details", &["event.day".into()]),
            NaiveDate::from_ymd_opt(1970, 1, 3)
        );
        assert_eq!(
            table.first_calendar_date("stamp", &[]),
            NaiveDate::from_ymd_opt(1969, 12, 31)
        );
        assert_eq!(
            table.first_calendar_date("details", &["event".into(), "day".into()]),
            None
        );
        Ok(())
    }

    #[test]
    fn nested_typed_arrays_and_binary_keep_shared_payload() {
        let photo = serde_json::json!({"bytes":[0,39,92,255],"path":"中文.png","items":[1,null,2]});
        let table = ArrowTable::from_cells(
            vec![
                column("photo", "STRUCT(bytes BLOB, path VARCHAR, items BIGINT[])"),
                column("mapping", "MAP(VARCHAR,BIGINT[])"),
                column("items", "BIGINT[]"),
            ],
            vec![vec![
                Cell {
                    json: Some(photo),
                    ..Default::default()
                },
                Cell {
                    json: Some(serde_json::json!({"A":[1,null],"B":[2,3]})),
                    ..Default::default()
                },
                Cell {
                    json: Some(serde_json::json!([1, null, 2])),
                    ..Default::default()
                },
            ]],
        );
        assert_eq!(table.binary(0, 0), Some(&[0, 39, 92, 255][..]));
        let clone = table.clone();
        assert_eq!(
            table.binary(0, 0).unwrap().as_ptr(),
            clone.binary(0, 0).unwrap().as_ptr()
        );
        let value = table.cell(0, 0).json.unwrap();
        assert_eq!(value["path"], "中文.png");
        assert_eq!(value["items"], serde_json::json!([1, null, 2]));
        assert_eq!(
            table.cell(0, 1).json.unwrap(),
            serde_json::json!({"A":[1,null],"B":[2,3]})
        );
        assert!(table.is_nested(0, 0));
        assert_eq!(table.display_text(0, 0), "Struct 3");
        assert_eq!(table.display_text(0, 1), "Map · 2 entries · Open table");
        assert_eq!(table.display_text(0, 2), "List 3");
        assert_eq!(table.cell(0, 2).text, "[1,null,2]");
        assert_eq!(
            table.preview_cell(0, 2, false).json.unwrap(),
            serde_json::json!([1, null, 2])
        );
    }

    #[test]
    fn complex_map_keys_keep_unambiguous_json_identity() {
        let table = ArrowTable::from_cells(
            vec![column("mapping", "MAP(VARCHAR[],INTEGER)")],
            vec![vec![Cell {
                json: Some(
                    serde_json::json!([{"key":["a","b"],"value":1},{"key":["a, b"],"value":2}]),
                ),
                ..Default::default()
            }]],
        );
        let value = table.cell(0, 0).json.unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(value["[\"a\",\"b\"]"], 1);
        assert_eq!(value["[\"a, b\"]"], 2);
    }

    #[test]
    fn json_export_preserves_duplicate_map_entries_and_json_logical_values() {
        let table = ArrowTable::from_cells(
            vec![column("mapping", "MAP(VARCHAR,INTEGER)")],
            vec![vec![Cell {
                json: Some(serde_json::json!([
                    {"key":"same","value":1},{"key":"same","value":2}
                ])),
                ..Default::default()
            }]],
        );
        let value = ExportFormatter::new("MAP(VARCHAR,INTEGER)")
            .json_value(table.raw_batches()[0].column(0).as_ref(), 0);
        assert_eq!(
            value,
            serde_json::json!([
                {"key":"same","value":1},{"key":"same","value":2}
            ])
        );
        let json = StringArray::from(vec![r#"{"exact":9007199254740993,"items":[true,null]}"#]);
        let value = ExportFormatter::new("JSON").json_value(&json, 0);
        assert_eq!(value["exact"].to_string(), "9007199254740993");
        assert_eq!(value["items"], serde_json::json!([true, null]));
    }

    #[test]
    fn preview_materializes_only_requested_payload_and_slice_outlives_owner() {
        let table = ArrowTable::from_cells(
            vec![
                column(
                    "photo",
                    "STRUCT(bytes BLOB, description VARCHAR, items INTEGER[])",
                ),
                column("exact", "BIGINT"),
            ],
            vec![vec![
                Cell {
                    json: Some(
                        serde_json::json!({"bytes":[137,80,78,71],"description":"中".repeat(100000),"items":[1,null]}),
                    ),
                    ..Default::default()
                },
                Cell {
                    text: "9007199254740993".into(),
                    ..Default::default()
                },
            ]],
        );
        let slice = table.slice_cell(0, 0).unwrap();
        assert_eq!(slice.row_count(), 1);
        assert_eq!(slice.data.columns.len(), 1);
        assert_eq!(
            table.binary(0, 0).unwrap().as_ptr(),
            slice.binary(0, 0).unwrap().as_ptr()
        );
        let image = table.preview_cell(0, 0, true);
        assert_eq!(image.bytes, Some(vec![137, 80, 78, 71]));
        assert!(image.json.is_none());
        assert!(image.text.len() < 100);
        let inspect = table.preview_cell(0, 0, false);
        assert!(inspect.text.is_empty());
        assert_eq!(inspect.json, table.cell(0, 0).json);
        assert_eq!(table.preview_cell(0, 1, false).text, "9007199254740993");
        let expected = table.cell(0, 0);
        drop(table);
        assert_eq!(slice.cell(0, 0).json, expected.json);
    }

    #[test]
    fn visible_unicode_is_bounded_but_copy_keeps_full_value() {
        let table = ArrowTable::from_cells(
            vec![column("text", "VARCHAR")],
            vec![vec![Cell {
                text: "中😀".repeat(100000),
                ..Default::default()
            }]],
        );
        assert_eq!(table.display_text(0, 0).chars().count(), 2000);
        assert_eq!(table.cell(0, 0).text.chars().count(), 200000);
        assert!(!table.is_nested(0, 0));
    }

    #[test]
    fn index_views_share_base_and_map_cells_provenance_and_export_order() {
        let columns = vec![column("id", "BIGINT"), column("image", "BLOB")];
        let batches = [0, 2, 4]
            .into_iter()
            .map(|first| {
                RecordBatch::try_from_iter([
                    (
                        "id",
                        Arc::new(Int64Array::from(vec![first, first + 1])) as ArrayRef,
                    ),
                    (
                        "image",
                        Arc::new(BinaryArray::from_vec(vec![
                            &[first as u8][..],
                            &[(first + 1) as u8][..],
                        ])) as ArrayRef,
                    ),
                ])
                .unwrap()
            })
            .collect();
        let base = ArrowTable::new(columns, batches)
            .unwrap()
            .with_source_row_start(101);
        let view = base
            .with_row_indices(UInt64Array::from(vec![5, 0, 2]))
            .unwrap();
        assert!(Arc::ptr_eq(&base.data, &view.data));
        assert_eq!(view.row_count(), 3);
        assert_eq!(
            (0..3)
                .map(|row| view.source_row_number(row).unwrap())
                .collect::<Vec<_>>(),
            [106, 101, 103]
        );
        assert_eq!(view.cell(0, 0).text, "5");
        assert_eq!(
            view.binary(0, 1).unwrap().as_ptr(),
            base.binary(5, 1).unwrap().as_ptr()
        );
        let slice = view.slice_cell(0, 1).unwrap();
        assert_eq!(slice.source_row_number(0), Some(106));
        assert_eq!(
            slice.binary(0, 0).unwrap().as_ptr(),
            base.binary(5, 1).unwrap().as_ptr()
        );
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let output = view
            .export_batches(2, &cancel)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            output.iter().map(RecordBatch::num_rows).collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(
            output[0]
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values(),
            &[5, 0]
        );
        assert_eq!(
            output[1]
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values(),
            &[2]
        );
        assert_eq!(
            view.with_row_indices(UInt64Array::from(vec![4]))
                .unwrap()
                .cell(0, 0)
                .text,
            "4",
            "indices always address base, never the old view"
        );
        assert!(base.with_row_indices(UInt64Array::from(vec![6])).is_err());
        assert!(
            base.with_row_indices(UInt64Array::from(vec![None]))
                .is_err()
        );
        let empty = base
            .with_row_indices(UInt64Array::from(Vec::<u64>::new()))
            .unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.schema(), base.schema());
        assert!(empty.export_batches(10, &cancel).next().is_none());
        drop(base);
        drop(view);
        assert_eq!(slice.binary(0, 0), Some(&[5][..]));
    }

    #[test]
    fn streamed_index_chunks_share_buffers_and_preserve_boundary_lookups_and_exports() -> Result<()>
    {
        let columns = vec![column("id", "BIGINT"), column("text", "VARCHAR")];
        let batches = [0..3, 3..7]
            .into_iter()
            .map(|ids| {
                RecordBatch::try_from_iter([
                    (
                        "id",
                        Arc::new(Int64Array::from_iter_values(ids.clone())) as ArrayRef,
                    ),
                    (
                        "text",
                        Arc::new(StringArray::from_iter_values(
                            ids.map(|id| format!("row-{id}")),
                        )) as ArrayRef,
                    ),
                ])
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let base = ArrowTable::new(columns, batches)?.with_source_row_start(101);
        let first = UInt64Array::from(vec![6, 0]);
        let middle = UInt64Array::from(vec![4]);
        let last = UInt64Array::from(vec![2, 5, 1, 0]);
        let pointers = [
            first.values().as_ptr().cast::<u8>(),
            middle.values().as_ptr().cast::<u8>(),
            last.values().as_ptr().cast::<u8>(),
        ];
        let view = base.with_row_index_batches(vec![
            UInt64Array::from(Vec::<u64>::new()),
            first,
            middle,
            UInt64Array::from(Vec::<u64>::new()),
            last,
        ])?;
        let retained = view.indices.as_ref().unwrap();
        assert_eq!(retained.chunks.len(), 3);
        assert_eq!(
            retained
                .chunks
                .iter()
                .map(|chunk| chunk.buffer().unwrap().as_ptr())
                .collect::<Vec<_>>(),
            pointers
        );
        let order = [6i64, 0, 4, 2, 5, 1, 0];
        for (row, id) in order.iter().enumerate() {
            assert_eq!(view.cell(row, 0).text, id.to_string());
            assert_eq!(view.source_row_number(row), Some(101 + *id as usize));
        }
        assert_eq!(view.source_row_number(order.len()), None);
        // Both chunk boundaries and the underlying source batch boundary are
        // crossed, with Arrow interleave retaining the exact logical row order.
        let projection = view.project_columns(&[1, 0])?;
        let materialized = projection.materialize_range(1..5)?;
        let ids = materialized
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(ids.values(), &[0, 4, 2, 5]);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let exported = view
            .export_batches(2, &cancel)
            .collect::<Result<Vec<_>>>()?;
        let exported_ids = exported
            .iter()
            .flat_map(|batch| {
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .values()
                    .iter()
                    .copied()
            })
            .collect::<Vec<_>>();
        assert_eq!(exported_ids, order);
        let contiguous = base
            .with_row_index_batches(vec![
                UInt64Array::from(vec![0]),
                UInt64Array::from(vec![1, 2]),
            ])?
            .project_columns(&[1, 0])?;
        let slice = contiguous.materialize_range(0..3)?;
        let source_ids = base.raw_batches()[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(
            slice
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values()
                .as_ptr(),
            source_ids.values().as_ptr(),
            "Contiguous filtered export keeps source value buffers shared"
        );
        assert!(
            base.with_row_index_batches(vec![
                UInt64Array::from(vec![0]),
                UInt64Array::from(vec![None])
            ])
            .is_err()
        );
        assert!(
            base.with_row_index_batches(vec![
                UInt64Array::from(vec![0]),
                UInt64Array::from(vec![7])
            ])
            .is_err()
        );
        let empty = base.with_row_index_batches(vec![UInt64Array::from(Vec::<u64>::new())])?;
        assert_eq!(empty.row_count(), 0);
        assert_eq!(empty.schema(), base.schema());
        assert!(empty.export_batches(2, &cancel).next().is_none());
        let shared = UInt64Array::new(
            vec![6, 0, 4, 2, 5, 1, 0].into(),
            Some(arrow::buffer::NullBuffer::new_valid(7)),
        );
        let shared_view =
            base.with_row_index_batches(vec![shared.slice(0, 2), shared.slice(2, 5)])?;
        assert_eq!(
            shared_view.memory_size() - base.memory_size(),
            shared.get_buffer_memory_size()
                + 2 * (std::mem::size_of::<RowIndexChunk>()
                    + std::mem::size_of::<IndexChunkShape>()),
            "Shared value and all-valid null buffers are each charged only once"
        );
        Ok(())
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn compact_positions_preserve_wide_source_rows_and_compose_sorted_views() -> Result<()> {
        let limit = u32::MAX as usize + 1;
        assert_eq!(RowIndexWidth::for_rows(limit), RowIndexWidth::Compact);
        assert_eq!(RowIndexWidth::for_rows(limit + 1), RowIndexWidth::Wide);
        let source_row = limit + 19;
        let ids = Arc::new(Int64Array::from_iter_values(10..18)) as ArrayRef;
        let full = RecordBatch::try_from_iter([("id", ids)])?;
        let base = ArrowTable::new(
            vec![column("id", "BIGINT")],
            vec![full.slice(0, 3), full.slice(3, 5)],
        )?
        .with_source_row_start(source_row);
        // Both chunks touch the later source batch, so sparse materialization
        // concatenates compact chunks and subtracts the base batch offset.
        let view = base.with_row_index_batches(vec![
            UInt32Array::from(vec![7, 3]),
            UInt32Array::from(vec![5, 7, 4, 6]),
        ])?;
        assert_eq!(
            view.indices.as_ref().unwrap().chunks[0]
                .buffer()
                .unwrap()
                .len(),
            8
        );
        let gathered = view.materialize_range(0..6)?;
        assert_eq!(
            gathered
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values(),
            &[17, 13, 15, 17, 14, 16],
        );
        let selected = view.select_view_rows(vec![UInt64Array::from(vec![4, 0, 4, 1])])?;
        for (row, (value, original)) in [(14, 4), (17, 7), (14, 4), (13, 3)].into_iter().enumerate()
        {
            assert_eq!(selected.cell(row, 0).text, value.to_string());
            assert_eq!(selected.source_row_number(row), Some(source_row + original));
        }
        assert!(
            view.select_view_rows(vec![UInt64Array::from(vec![6])])
                .is_err()
        );
        assert!(
            view.select_view_rows(vec![UInt64Array::from(vec![None])])
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn indexed_export_shares_contiguous_runs_across_batch_and_index_boundaries() -> Result<()> {
        let array = Arc::new(Int64Array::from(vec![
            Some(0),
            None,
            Some(2),
            Some(3),
            None,
            Some(5),
            Some(6),
            Some(7),
        ])) as ArrayRef;
        let full = RecordBatch::try_from_iter([("id", array)])?;
        let base = ArrowTable::new(
            vec![column("id", "BIGINT")],
            vec![
                full.slice(0, 0),
                full.slice(0, 3),
                full.slice(3, 3),
                full.slice(6, 2),
            ],
        )?;
        // The first index chunk spans two imported batches, and the second
        // imported batch spans both index chunks. Neither boundary needs a copy.
        let view = base.with_row_index_batches(vec![
            RowIndexWidth::Compact.contiguous_positions(0..5),
            RowIndexWidth::Compact.contiguous_positions(5..8),
        ])?;
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let output = view
            .export_batches(4, &cancel)
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            output.iter().map(RecordBatch::num_rows).collect::<Vec<_>>(),
            [3, 2, 1, 2]
        );
        let mut first = 0;
        for batch in output {
            assert_eq!(batch, full.slice(first, batch.num_rows()));
            assert_eq!(
                batch.column(0).to_data().buffers()[0].data_ptr(),
                full.column(0).to_data().buffers()[0].data_ptr(),
                "continuous export keeps the original allocation shared"
            );
            first += batch.num_rows();
        }
        assert_eq!(first, 8);
        Ok(())
    }

    #[test]
    fn nested_sparse_materialize_preserves_null_and_duplicate_rows() -> Result<()> {
        use arrow::datatypes::{Field, Int32Type};
        let ids = Arc::new(Int64Array::from(vec![
            Some(0),
            None,
            Some(2),
            Some(3),
            None,
            Some(5),
            Some(6),
            Some(7),
        ])) as ArrayRef;
        let lists = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>([
            Some(vec![Some(0)]),
            None,
            Some(vec![None]),
            Some(vec![Some(3), None]),
            Some(Vec::new()),
            Some(vec![Some(5)]),
            None,
            Some(vec![Some(7)]),
        ])) as ArrayRef;
        let nested = Arc::new(StructArray::try_new(
            vec![Arc::new(Field::new(
                "items",
                lists.data_type().clone(),
                true,
            ))]
            .into(),
            vec![lists],
            Some(arrow::buffer::NullBuffer::from(vec![
                true, true, false, true, true, false, true, true,
            ])),
        )?) as ArrayRef;
        let full = RecordBatch::try_from_iter([("id", ids), ("nested", nested)])?;
        let base = ArrowTable::new(
            crate::arrow_types::columns_from_schema(&full.schema()),
            vec![full.slice(0, 3), full.slice(3, 5)],
        )?;
        // Both index chunks address the second base batch. The order includes
        // repeated rows, list NULLs and struct-parent NULLs.
        let view = base.with_row_index_batches(vec![
            UInt64Array::from(vec![7, 3]),
            UInt64Array::from(vec![5, 7, 4, 6]),
        ])?;
        let expected = arrow::compute::interleave_record_batch(
            &[&full],
            &[(0, 7), (0, 3), (0, 5), (0, 7), (0, 4), (0, 6)],
        )?;
        assert_eq!(view.materialize_range(0..6)?, expected);
        // A partial sparse chunk retains its own range semantics.
        assert_eq!(view.materialize_range(1..5)?, expected.slice(1, 4));
        assert_eq!(view.materialize_range(3..3)?.num_rows(), 0);
        assert!(view.materialize_range(0..7).is_err());
        assert!(
            view.materialize_range(std::ops::Range { start: 4, end: 3 })
                .is_err()
        );
        let projected = view.project_columns(&[])?;
        assert_eq!(projected.materialize_range(0..6)?.num_rows(), 6);
        assert_eq!(projected.materialize_range(0..6)?.num_columns(), 0);
        Ok(())
    }

    #[test]
    fn sparse_nested_slices_bound_child_buffers_to_the_selected_values() -> Result<()> {
        use arrow::buffer::OffsetBuffer;
        use arrow::datatypes::Field;
        let rows = 65_536usize;
        let child = Arc::new(Int64Array::from_iter_values(0..(rows * 2) as i64)) as ArrayRef;
        let list = Arc::new(ListArray::new(
            Arc::new(Field::new("item", DataType::Int64, false)),
            OffsetBuffer::new(
                (0..=rows)
                    .map(|row| (row * 2) as i32)
                    .collect::<Vec<_>>()
                    .into(),
            ),
            child.clone(),
            None,
        )) as ArrayRef;
        let nested = Arc::new(StructArray::from(vec![(
            Arc::new(Field::new("items", list.data_type().clone(), false)),
            list,
        )])) as ArrayRef;
        let full = RecordBatch::try_from_iter([("nested", nested)])?;
        let first = rows / 2;
        let base = ArrowTable::new(
            crate::arrow_types::columns_from_schema(&full.schema()),
            vec![full.slice(first, 32)],
        )?;
        let order = [31u64, 0, 17, 31, 9, 2, 17];
        let selected = base.with_row_indices(UInt64Array::from(order.to_vec()))?;
        let output = selected.materialize_range(0..order.len())?;
        let records = output
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let lists = records
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        for (row, index) in order.iter().enumerate() {
            let values = lists.value(row);
            let values = values.as_any().downcast_ref::<Int64Array>().unwrap();
            let expected = ((first as u64 + index) * 2) as i64;
            assert_eq!(values.values(), &[expected, expected + 1]);
        }
        let selected_bytes = order.len() * 2 * std::mem::size_of::<i64>();
        assert!(
            lists.values().get_buffer_memory_size() <= selected_bytes + 1024,
            "A small sparse export must size its output children from selected values"
        );
        assert!(child.get_buffer_memory_size() > 1024 * selected_bytes);
        Ok(())
    }

    #[test]
    fn plain_export_slices_do_not_copy_and_cancellation_stops_once() {
        let table = ArrowTable::from_cells(
            vec![column("id", "BIGINT")],
            (0..5)
                .map(|id| {
                    vec![Cell {
                        text: id.to_string(),
                        ..Default::default()
                    }]
                })
                .collect(),
        );
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut export = table.export_batches(2, &cancel);
        let first = export.next().unwrap().unwrap();
        assert_eq!(
            first.column(0).to_data().buffers()[0].as_ptr(),
            table.raw_batches()[0].column(0).to_data().buffers()[0].as_ptr()
        );
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert!(export.next().unwrap().is_err());
        assert!(export.next().is_none());
    }

    #[test]
    fn dictionary_access_follows_one_key_and_preserves_numeric_json_and_nulls() {
        use arrow::datatypes::Int8Type;
        let values = Arc::new(Int64Array::from(vec![Some(9007199254740993), None])) as ArrayRef;
        let dictionary = DictionaryArray::<Int8Type>::try_new(
            Int8Array::from(vec![Some(0), Some(1), None]),
            values,
        )
        .unwrap();
        let batch =
            RecordBatch::try_from_iter([("value", Arc::new(dictionary) as ArrayRef)]).unwrap();
        let table = ArrowTable::new(vec![column("value", "BIGINT")], vec![batch]).unwrap();
        assert_eq!(table.cell(0, 0).text, "9007199254740993");
        assert!(table.is_null(1, 0));
        assert!(table.is_null(2, 0));
        assert_eq!(
            ExportFormatter::new("BIGINT")
                .json_value(table.raw_batches()[0].column(0).as_ref(), 0)
                .to_string(),
            "9007199254740993"
        );
        assert_eq!(
            table.slice_cell(0, 0).unwrap().cell(0, 0).text,
            "9007199254740993"
        );
    }

    #[test]
    fn projected_view_shares_selected_buffers_and_drops_hidden_payload() -> Result<()> {
        use arrow::datatypes::{Field, Schema};
        let hidden = Arc::new(BinaryArray::from(vec![Some(&vec![7u8; 262_144][..]); 3]));
        let hidden_observer = Arc::downgrade(&hidden);
        let span = Field::new("span", DataType::FixedSizeBinary(12), true).with_metadata(
            [(
                "parquetviewer.logical_type".into(),
                "INTERVAL_MONTH_DAY_MILLISECOND_LE".into(),
            )]
            .into_iter()
            .collect(),
        );
        let schema = Arc::new(Schema::new_with_metadata(
            vec![
                Field::new("id", DataType::Int64, false),
                Field::new("hidden", DataType::Binary, true),
                span.clone(),
            ],
            [("fixture".into(), "metadata".into())]
                .into_iter()
                .collect(),
        ));
        let intervals = Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            [
                Some(&[0u8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0][..]),
                None,
                Some(&[1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..]),
            ]
            .into_iter(),
            12,
        )?) as ArrayRef;
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![10, 11, 12])),
                hidden.clone(),
                intervals.clone(),
            ],
        )?;
        let base = ArrowTable::new(
            crate::arrow_types::columns_from_schema(&schema),
            vec![batch],
        )?
        .with_source_row_start(41);
        drop(hidden);
        let view = base.with_row_indices(UInt64Array::from(vec![2, 0, 1]))?;
        let projected = view.project_columns(&[2, 0])?;
        assert_eq!(projected.schema().metadata(), schema.metadata());
        assert_eq!(projected.schema().field(0), &span);
        assert!(Arc::ptr_eq(
            projected.raw_batches()[0].column(0),
            &intervals
        ));
        assert!(Arc::ptr_eq(
            projected.indices.as_ref().unwrap(),
            view.indices.as_ref().unwrap()
        ));
        assert_eq!(
            (0..3)
                .map(|row| projected.source_row_number(row))
                .collect::<Vec<_>>(),
            [Some(43), Some(41), Some(42)]
        );
        assert_eq!(projected.cell(0, 0).text, "1 month");
        assert_eq!(projected.cell(1, 0).text, "00:00:00.001");
        assert!(projected.is_null(2, 0));
        assert!(projected.memory_size() < 4096);
        assert!(view.project_columns(&[0, 0]).is_err());
        assert!(view.project_columns(&[3]).is_err());
        let no_columns = projected.project_columns(&[])?;
        assert_eq!(no_columns.row_count(), 3);
        assert_eq!(no_columns.raw_batches()[0].num_rows(), 3);
        assert_eq!(no_columns.materialize_range(0..3)?.num_rows(), 3);
        assert_eq!(no_columns.schema().metadata(), schema.metadata());
        drop(base);
        drop(view);
        assert!(
            hidden_observer.upgrade().is_none(),
            "projection must not retain the original wide TableData"
        );
        let gathered = projected.materialize_range(0..3)?;
        assert_eq!(gathered.num_columns(), 2);
        assert_eq!(
            gathered
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values()
                .as_ref(),
            &[12, 10, 11]
        );
        Ok(())
    }

    #[test]
    fn merge_aligns_different_chunks_and_keeps_both_leases() -> Result<()> {
        use arrow::datatypes::{Field, Schema};
        let client = crate::resource_budget::Client::new();
        let quota = crate::resource_budget::ACTIVE_CACHE_BYTES;
        let id = Arc::new(Int64Array::from(vec![10, 11, 12, 13, 14])) as ArrayRef;
        let id_buffer = id.to_data().buffers()[0].data_ptr();
        let left_schema = Arc::new(Schema::new_with_metadata(
            vec![Field::new("id", DataType::Int64, false)],
            [
                ("shared".into(), "same".into()),
                ("left".into(), "one".into()),
            ]
            .into_iter()
            .collect(),
        ));
        let left_batch = RecordBatch::try_new(left_schema.clone(), vec![id])?;
        let mut left = ArrowTable::new(
            vec![column("id", "BIGINT")],
            vec![
                left_batch.slice(0, 2),
                left_batch.slice(2, 0),
                left_batch.slice(2, 3),
            ],
        )?
        .with_source_row_start(101);
        left.retain_memory(client.try_reserve_cache(quota / 2).unwrap());
        left.retain_memory(client.reserve_display(left.memory_size())?);
        let json_field = Field::new("document", DataType::Utf8, true).with_metadata(
            [
                ("ARROW:extension:name".into(), "arrow.json".into()),
                ("ARROW:extension:metadata".into(), String::new()),
            ]
            .into_iter()
            .collect(),
        );
        let right_schema = Arc::new(Schema::new_with_metadata(
            vec![json_field.clone()],
            [
                ("shared".into(), "same".into()),
                ("right".into(), "two".into()),
            ]
            .into_iter()
            .collect(),
        ));
        let right_batch = RecordBatch::try_new(
            right_schema.clone(),
            vec![Arc::new(StringArray::from(vec![
                Some("{}"),
                None,
                Some("[1]"),
                Some("true"),
                Some("42"),
            ]))],
        )?;
        let mut right = ArrowTable::new(
            vec![column("document", "JSON")],
            vec![right_batch.slice(0, 1), right_batch.slice(1, 4)],
        )?
        .with_source_row_start(101);
        right.retain_memory(client.try_reserve_cache(quota / 2).unwrap());
        right.retain_memory(client.reserve_display(right.memory_size())?);
        let merged = left.merge_columns(&right)?;
        assert_eq!(
            merged
                .raw_batches()
                .iter()
                .map(RecordBatch::num_rows)
                .collect::<Vec<_>>(),
            [1, 1, 3]
        );
        assert_eq!(merged.schema().field(1), &json_field);
        assert_eq!(
            merged.schema().metadata().get("left").map(String::as_str),
            Some("one")
        );
        assert_eq!(
            merged.schema().metadata().get("right").map(String::as_str),
            Some("two")
        );
        assert_eq!(
            merged.memory_size(),
            left.memory_size() + right.memory_size()
        );
        for batch in merged.raw_batches() {
            assert_eq!(batch.column(0).to_data().buffers()[0].data_ptr(), id_buffer);
        }
        assert!(Arc::ptr_eq(
            merged.raw_batches()[2].column(0),
            left.raw_batches()[2].column(0)
        ));
        assert_eq!(
            (0..5)
                .map(|row| merged.source_row_number(row))
                .collect::<Vec<_>>(),
            [Some(101), Some(102), Some(103), Some(104), Some(105)]
        );
        let projected = merged.project_columns(&[1])?;
        assert_eq!(projected.memory_size(), right.memory_size());
        let display_charges = projected
            .data
            .leases
            .iter()
            .filter(|lease| lease.is_display())
            .map(|lease| lease.bytes())
            .collect::<Vec<_>>();
        assert_eq!(display_charges, [right.memory_size()]);
        let retained = projected.slice_cell(2, 0).unwrap();
        drop(left);
        drop(right);
        drop(merged);
        drop(projected);
        assert!(
            client.try_reserve_cache(1).is_none(),
            "both imports' leases must follow the last projected slice"
        );
        assert_eq!(retained.cell(0, 0).text, "[1]");
        drop(retained);
        assert!(client.try_reserve_cache(quota).is_some());
        Ok(())
    }

    #[test]
    fn merge_checks_provenance_and_preserves_noncontiguous_source_ids() -> Result<()> {
        fn explicit(name: &str, split: usize, source_ids: Vec<u64>) -> Result<ArrowTable> {
            let batch = RecordBatch::try_from_iter([(
                name,
                Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef,
            )])?;
            let mut table = ArrowTable::new(
                vec![column(name, "BIGINT")],
                vec![batch.slice(0, split), batch.slice(split, 3 - split)],
            )?;
            let numbers = UInt64Array::from(source_ids);
            Arc::get_mut(&mut table.data).unwrap().source_row_numbers =
                vec![numbers.slice(0, split), numbers.slice(split, 3 - split)];
            Ok(table)
        }
        let left = explicit("a", 2, vec![10, 20, 40])?;
        let right = explicit("b", 1, vec![10, 20, 40])?;
        let merged = left.merge_columns(&right)?;
        assert_eq!(
            (0..3)
                .map(|row| merged.source_row_number(row))
                .collect::<Vec<_>>(),
            [Some(10), Some(20), Some(40)]
        );
        let view = merged
            .project_columns(&[1])?
            .with_row_indices(UInt64Array::from(vec![2, 0]))?;
        assert_eq!(view.source_row_number(0), Some(40));
        assert_eq!(view.source_row_number(1), Some(10));
        assert!(
            left.merge_columns(&explicit("b", 1, vec![10, 20, 41])?)
                .is_err()
        );
        assert!(left.merge_columns(&left).is_err());
        assert!(view.merge_columns(&right).is_err());
        let known = explicit("c", 1, vec![10, 20, 40])?.with_source_row_start(1);
        let other = explicit("d", 1, vec![10, 20, 40])?.with_source_row_start(2);
        assert!(known.merge_columns(&other).is_err());
        let unknown = ArrowTable::new(
            vec![column("d", "BIGINT")],
            vec![RecordBatch::try_from_iter([(
                "d",
                Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef,
            )])?],
        )?;
        assert!(left.merge_columns(&unknown).is_err());
        Ok(())
    }

    #[test]
    fn empty_project_and_merge_preserve_typed_metadata_and_reject_conflicts() -> Result<()> {
        use arrow::datatypes::{Field, Schema};
        let empty = |name: &str, metadata: &str| {
            ArrowTable::new_with_schema(
                vec![column(name, "BIGINT")],
                Vec::new(),
                Arc::new(Schema::new_with_metadata(
                    vec![Field::new(name, DataType::Int64, true)],
                    [("same".into(), metadata.into())].into_iter().collect(),
                )),
            )
            .map(|table| table.with_source_row_start(55))
        };
        let left = empty("a", "value")?;
        let right = empty("b", "value")?;
        let merged = left.merge_columns(&right)?;
        let projected = merged.project_columns(&[1, 0])?;
        assert!(projected.is_empty());
        assert_eq!(projected.schema().field(0).name(), "b");
        assert_eq!(
            projected
                .schema()
                .metadata()
                .get("same")
                .map(String::as_str),
            Some("value")
        );
        assert_eq!(projected.data.source_row_start, Some(55));
        assert!(left.merge_columns(&empty("b", "conflict")?).is_err());
        let nonempty = ArrowTable::new(
            vec![column("b", "BIGINT")],
            vec![RecordBatch::try_from_iter([(
                "b",
                Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            )])?],
        )?
        .with_source_row_start(55);
        assert!(left.merge_columns(&nonempty).is_err());
        Ok(())
    }

    #[test]
    fn nested_map_sort_keys_share_all_buffers_including_slices_and_nulls() {
        use arrow::datatypes::{Field, Int8Type};
        use serde_json::json;

        fn same_buffers(before: &ArrayData, after: &ArrayData) {
            assert_eq!(before.len(), after.len());
            assert_eq!(before.offset(), after.offset());
            assert_eq!(before.nulls(), after.nulls());
            assert_eq!(
                before.nulls().map(|nulls| nulls.buffer().as_ptr()),
                after.nulls().map(|nulls| nulls.buffer().as_ptr())
            );
            assert_eq!(before.buffers().len(), after.buffers().len());
            for (before, after) in before.buffers().iter().zip(after.buffers()) {
                assert_eq!(before.as_ptr(), after.as_ptr());
            }
            for (before, after) in before.child_data().iter().zip(after.child_data()) {
                same_buffers(before, after);
            }
        }

        let map = fixture_schema(&[column("map", "MAP(VARCHAR, INTEGER)")])
            .unwrap()
            .field(0)
            .data_type()
            .clone();
        let cases = [
            (
                map.clone(),
                vec![json!({"b": 2}), Value::Null, json!({}), json!({"a": null})],
            ),
            (
                DataType::List(Arc::new(Field::new("item", map.clone(), true))),
                vec![json!([{"b": 2}, null]), Value::Null, json!([]), json!([{}])],
            ),
            (
                DataType::FixedSizeList(Arc::new(Field::new("item", map.clone(), true)), 1),
                vec![json!([{"b": 2}]), Value::Null, json!([{}]), json!([null])],
            ),
            (
                DataType::Struct(vec![Field::new("map", map.clone(), true)].into()),
                vec![
                    json!({"map": {"b": 2}}),
                    Value::Null,
                    json!({"map": {}}),
                    json!({"map": null}),
                ],
            ),
        ];
        for (kind, values) in cases {
            let original = crate::test_support::array_from_json(&kind, &values).unwrap();
            let original = original.slice(1, 3);
            let sorted = sortable_array(&original).unwrap();
            assert_eq!(sorted.data_type(), &sortable_type(&kind));
            same_buffers(&original.to_data(), &sorted.to_data());
            let converter = arrow::row::RowConverter::new(vec![arrow::row::SortField::new(
                sorted.data_type().clone(),
            )])
            .unwrap();
            assert_eq!(converter.convert_columns(&[sorted]).unwrap().num_rows(), 3);
            assert_eq!(original.data_type(), &kind);
        }

        let values = crate::test_support::array_from_json(
            &map,
            &[json!({"b": 2}), Value::Null, json!({"a": 1})],
        )
        .unwrap();
        let dictionary = Arc::new(
            DictionaryArray::<Int8Type>::try_new(
                Int8Array::from(vec![Some(2), None, Some(1), Some(0)]),
                values,
            )
            .unwrap(),
        ) as ArrayRef;
        let sorted = sortable_array(&dictionary).unwrap();
        same_buffers(&dictionary.to_data(), &sorted.to_data());
        assert_eq!(sorted.data_type(), &sortable_type(dictionary.data_type()));
        let scalar = Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef;
        assert!(Arc::ptr_eq(&scalar, &sortable_array(&scalar).unwrap()));
    }
}
