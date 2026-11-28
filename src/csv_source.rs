//! Delimited text records become Arrow batches without a temporary dataset on disk.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{File, Metadata},
    io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
use arrow::{
    array::{
        Array, ArrayRef, BooleanArray, Decimal128Array, Decimal256Array, StringViewArray,
        TimestampMicrosecondArray, UInt64Array,
    },
    compute::{
        filter_record_batch,
        kernels::cast_utils::{Parser, parse_decimal, string_to_datetime},
        kernels::filter::prep_null_mask_filter,
    },
    csv::ReaderBuilder as ArrowCsvReader,
    datatypes::{
        DataType, Date32Type, Decimal128Type, Decimal256Type, Field, Float64Type, Int64Type,
        Schema, SchemaRef, TimeUnit, UInt64Type,
    },
    record_batch::RecordBatch,
};
use csv::{ByteRecord, Position, Reader, ReaderBuilder};
use datafusion::physical_expr::{
    PhysicalExpr,
    utils::{collect_columns, reassign_expr_columns},
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    arrow_types::columns_from_schema,
    columnar::ArrowTable,
    engine::{ColumnInfo, DatasetInfo},
    resource_budget::Client,
    source_loader::SourceStamps,
};

const DEFAULT_SAMPLE_RECORDS: usize = 1_000;
const BROWSE_SAMPLE_BYTES: u64 = 256 * 1024;
const MAX_SAMPLE_RECORDS: usize = 100_000;
const DIALECT_BYTES: usize = 128 * 1024;
const PREVIEW_ROWS: usize = 12;
const PREVIEW_CHARS: usize = 512;
const PREVIEW_BYTES: usize = 512 * 1024;
const RECORD_BYTES: usize = 64 * 1024 * 1024;
const RECORD_SIZE_ERROR: &str = "A text record exceeds the 64 MiB limit.";
const INDEX_ROWS: usize = 16_384;
const INDEX_BYTES: u64 = 1024 * 1024;
const READ_WAVE_BYTES: u64 = 8 * INDEX_BYTES;
const BATCH_ROWS: usize = 8_192;
const DECODE_BATCH_BYTES: u64 = 1024 * 1024;
const BUFFER_BYTES: usize = 64 * 1024;
// Waves end at a logical record boundary, and csv::Reader may read ahead.
// Reserve room for both so ordinary waves do not double their capacity.
const CAPTURE_BYTES: usize = (READ_WAVE_BYTES + INDEX_BYTES) as usize + BUFFER_BYTES;
const LARGE_RECORD_SCRATCH_BYTES: usize = 8 * 1024 * 1024;
const MIN_TEXT_BUFFER_SAVINGS: usize = 32 * 1024;

#[path = "csv_row_index.rs"]
mod row_index;
pub(crate) use row_index::CsvBrowse;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CsvIndexProgress {
    pub indexed_rows: usize,
    pub indexed_bytes: u64,
    pub total_bytes: u64,
    pub complete: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum CsvColumnType {
    Text,
    Boolean,
    Integer,
    Decimal,
    Float,
    Date,
    DateTime,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct CsvOptions {
    pub delimiter: u8,
    pub has_header: bool,
    pub infer_types: bool,
    pub sample_records: usize,
    pub column_types: BTreeMap<usize, CsvColumnType>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            has_header: true,
            infer_types: true,
            sample_records: DEFAULT_SAMPLE_RECORDS,
            column_types: BTreeMap::new(),
        }
    }
}

pub(crate) struct CsvProbe {
    pub options: CsvOptions,
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<String>>,
    pub sampled_records: usize,
    prepared: CsvPrepared,
}

#[derive(Clone)]
pub(crate) struct CsvPrepared {
    path: PathBuf,
    stamp: Stamp,
    options: CsvOptions,
    schema: SchemaRef,
    index: Index,
}

#[derive(Clone, Copy)]
enum SamplePurpose<'a> {
    Load,
    Browse,
    Preview,
    Reload(&'a HashMap<String, CsvColumnType>),
}

impl CsvProbe {
    pub(crate) fn read_for_browse(path: &Path, cancel: &AtomicBool) -> Result<Self> {
        Self::read_configured(path, None, SamplePurpose::Browse, cancel)
    }

    pub(crate) fn read(
        path: &Path,
        options: Option<CsvOptions>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::read_configured(path, options, SamplePurpose::Preview, cancel)
    }

    pub(crate) fn read_for_reload(
        path: &Path,
        options: CsvOptions,
        previous_types: &HashMap<String, CsvColumnType>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::read_configured(
            path,
            Some(options),
            SamplePurpose::Reload(previous_types),
            cancel,
        )
    }

    fn read_configured(
        path: &Path,
        options: Option<CsvOptions>,
        purpose: SamplePurpose<'_>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("Could not open {}", path.display()))?;
        let stamp = Stamp::from_metadata(&file.metadata()?);
        Self::read_sample(path, file, stamp, options, purpose, cancel)
    }

    pub(crate) fn read_detected(
        path: &Path,
        mut options: CsvOptions,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut file =
            File::open(path).with_context(|| format!("Could not open {}", path.display()))?;
        let stamp = Stamp::from_metadata(&file.metadata()?);
        options.delimiter = detect_delimiter(&mut file, default_delimiter(path), cancel)?;
        Self::read_sample(
            path,
            file,
            stamp,
            Some(options),
            SamplePurpose::Preview,
            cancel,
        )
    }

    #[allow(dead_code)] // The standalone benchmarks open sources without an interactive preview.
    pub(crate) fn prepared(&self) -> CsvPrepared {
        self.prepared.clone()
    }

    fn read_sample(
        path: &Path,
        file: File,
        stamp: Stamp,
        options: Option<CsvOptions>,
        purpose: SamplePurpose<'_>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let sample = Sample::read(
            file,
            stamp.len,
            options,
            default_delimiter(path),
            purpose,
            cancel,
            |_, _| {},
        )?;
        let path = path.canonicalize()?;
        stamp.validate_path(&path)?;
        let columns = columns_from_schema(&sample.schema);
        let sampled_records = sample.index.next.row;
        Ok(Self {
            options: sample.options.clone(),
            columns,
            rows: sample.preview,
            sampled_records,
            prepared: CsvPrepared {
                path,
                stamp,
                options: sample.options,
                schema: sample.schema,
                index: sample.index,
            },
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    identity: (u64, u64),
}

impl Stamp {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            identity: (metadata.dev(), metadata.ino()),
        }
    }

    fn validate_path(&self, path: &Path) -> Result<()> {
        let metadata = path
            .metadata()
            .context("Source changed after loading. Reload the file.")?;
        ensure!(
            Self::from_metadata(&metadata) == *self,
            "Source changed after loading. Reload the file."
        );
        Ok(())
    }

    fn open(&self, path: &Path) -> Result<File> {
        let file = File::open(path).context("Source changed after loading. Reload the file.")?;
        ensure!(
            Self::from_metadata(&file.metadata()?) == *self,
            "Source changed after loading. Reload the file."
        );
        Ok(file)
    }
}

// The CSV parser can read ahead. Reset this allowance before each logical
// record, rather than confusing physical newlines with record boundaries.
struct RecordReader {
    file: File,
    allowance: usize,
    captured: Option<Vec<u8>>,
}

impl RecordReader {
    fn new(file: File) -> Self {
        Self {
            file,
            allowance: RECORD_BYTES,
            captured: None,
        }
    }

    fn advance_capture(&mut self, consumed: usize, next_capacity: usize) {
        let bytes = self.captured.as_mut().expect("CSV capture has started");
        if bytes.capacity() > CAPTURE_BYTES {
            // Keep only the parser's unread tail after an unusually large row.
            let mut next = Vec::with_capacity(next_capacity);
            next.extend_from_slice(&bytes[consumed..]);
            *bytes = next;
        } else {
            bytes.copy_within(consumed.., 0);
            bytes.truncate(bytes.len() - consumed);
        }
    }
}

impl Read for RecordReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let limit = buffer.len().min(self.allowance);
        if limit == 0 {
            // An unterminated final record can fill the entire allowance. CSV
            // needs to observe EOF before returning its last field.
            if self.file.stream_position()? == self.file.metadata()?.len() {
                return Ok(0);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                RECORD_SIZE_ERROR,
            ));
        }
        let read = self.file.read(&mut buffer[..limit])?;
        self.allowance -= read;
        if let Some(captured) = &mut self.captured {
            captured.extend_from_slice(&buffer[..read]);
        }
        Ok(read)
    }
}

impl Seek for RecordReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.allowance = RECORD_BYTES;
        self.file.seek(position)
    }
}

fn parser<R: Read>(reader: R, delimiter: u8, header: bool, flexible: bool) -> Reader<R> {
    ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(header)
        .flexible(flexible)
        .buffer_capacity(BUFFER_BYTES)
        .from_reader(reader)
}

fn read_record(
    reader: &mut Reader<RecordReader>,
    record: &mut ByteRecord,
    start: u64,
) -> Result<bool> {
    reader.get_mut().allowance = RECORD_BYTES;
    let present = reader.read_byte_record(record)?;
    // The parser may already have buffered part of this record while reading
    // its predecessor. Apply the same size limit to indexed and fresh seeks.
    ensure!(
        !present || reader.position().byte() - start <= RECORD_BYTES as u64,
        RECORD_SIZE_ERROR
    );
    Ok(present)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    Ok(())
}

#[derive(Default)]
struct RecordScratch {
    record: ByteRecord,
    large: bool,
}

impl RecordScratch {
    fn trim(&mut self) {
        let bytes = self.record.as_slice().len();
        if bytes >= LARGE_RECORD_SCRATCH_BYTES {
            self.large = true;
        } else if self.large && bytes <= BUFFER_BYTES {
            self.record = ByteRecord::with_capacity(BUFFER_BYTES, self.record.len());
            self.large = false;
        }
    }
}

pub(crate) fn valid_delimiter(delimiter: u8) -> bool {
    delimiter == b'\t' || (b' '..=b'~').contains(&delimiter) && delimiter != b'"'
}

fn default_delimiter(path: &Path) -> u8 {
    let extension = path.extension().unwrap_or_default();
    if extension.eq_ignore_ascii_case("tsv") || extension.eq_ignore_ascii_case("tab") {
        b'\t'
    } else if extension.eq_ignore_ascii_case("psv") {
        b'|'
    } else {
        b','
    }
}

#[allow(dead_code)] // Version comparison is not included in the standalone benchmarks.
pub(crate) fn detect_path_delimiter(
    path: &Path,
    preferred: Option<u8>,
    cancel: &AtomicBool,
) -> Result<u8> {
    let fallback = preferred
        .filter(|&delimiter| valid_delimiter(delimiter))
        .unwrap_or_else(|| default_delimiter(path));
    let mut file =
        File::open(path).with_context(|| format!("Could not open {}", path.display()))?;
    detect_delimiter(&mut file, fallback, cancel)
}

fn detect_delimiter(file: &mut File, fallback: u8, cancel: &AtomicBool) -> Result<u8> {
    check_cancel(cancel)?;
    let mut prefix = Vec::with_capacity(DIALECT_BYTES);
    (&mut *file)
        .take(DIALECT_BYTES as u64)
        .read_to_end(&mut prefix)?;
    file.seek(SeekFrom::Start(0))?;
    let mut best = (0, 0, false, 0);
    let mut delimiter = fallback;
    let standard = [b',', b'\t', b';', b'|'];
    for candidate in standard
        .into_iter()
        .chain((!standard.contains(&fallback)).then_some(fallback))
    {
        check_cancel(cancel)?;
        let mut reader = parser(prefix.as_slice(), candidate, false, true);
        let mut record = ByteRecord::new();
        let mut widths = BTreeMap::<usize, usize>::new();
        let mut total = 0;
        while total < 20 && reader.read_byte_record(&mut record)? {
            total += 1;
            *widths.entry(record.len()).or_default() += 1;
        }
        for (width, count) in widths {
            if width > 1 && count * 2 > total {
                let score = (count * 100 / total, count, candidate == fallback, width);
                if score > best {
                    best = score;
                    delimiter = candidate;
                }
            }
        }
    }
    Ok(delimiter)
}

fn field_names(record: &ByteRecord, header: bool) -> Result<Vec<String>> {
    let mut used = HashSet::new();
    let mut names = Vec::with_capacity(record.len());
    for (index, raw) in record.iter().enumerate() {
        let original = if header {
            std::str::from_utf8(raw).context("Text file headers must contain valid UTF-8")?
        } else {
            ""
        };
        ensure!(
            !original.contains('\0'),
            "Text file field names cannot contain NUL"
        );
        let name = if original.trim().is_empty() {
            format!("column_{}", index + 1)
        } else {
            original.into()
        };
        let mut unique = name.clone();
        let mut suffix = 2;
        while !used.insert(unique.to_uppercase()) {
            unique = format!("{name} ({suffix})");
            suffix += 1;
        }
        names.push(unique);
    }
    Ok(names)
}

// Precision is tracked in decimal digits, without passing through Float64.
#[derive(Clone, Copy, Debug)]
struct DecimalShape {
    integer_digits: usize,
    fractional_digits: usize,
    leading_zero: bool,
    significant_digits: usize,
}

fn decimal_shape(value: &str) -> Option<DecimalShape> {
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (integer, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if unsigned == "."
        || unsigned.is_empty()
        || !integer
            .bytes()
            .chain(fraction.bytes())
            .all(|digit| digit.is_ascii_digit())
    {
        return None;
    }
    let trimmed_integer = integer.trim_start_matches('0');
    let trimmed_fraction = fraction.trim_end_matches('0');
    Some(DecimalShape {
        integer_digits: trimmed_integer.len(),
        fractional_digits: trimmed_fraction.len(),
        leading_zero: integer.len() > 1 && integer.starts_with('0'),
        significant_digits: if trimmed_integer.is_empty() {
            trimmed_fraction.trim_start_matches('0').len()
        } else {
            trimmed_integer.len() + trimmed_fraction.len()
        },
    })
}

#[derive(Clone, Copy, Debug, Default)]
enum Inferred {
    #[default]
    Unknown,
    Text,
    Boolean,
    Date,
    DateTime {
        nanos: bool,
        zoned: bool,
    },
    Float,
    Number {
        integer: usize,
        fractional: usize,
        signed: bool,
        i64: bool,
        u64: bool,
        decimal: bool,
        fits_float: bool,
    },
}

impl Inferred {
    fn observe(&mut self, value: &str) {
        if !value.is_empty() && !matches!(self, Self::Text) {
            *self = self.merge(Self::of(value));
        }
    }

    fn of(value: &str) -> Self {
        if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
            return Self::Boolean;
        }
        if let Some(shape) = decimal_shape(value) {
            if shape.leading_zero
                || shape.integer_digits.saturating_add(shape.fractional_digits) > 76
            {
                return Self::Text;
            }
            return Self::Number {
                integer: shape.integer_digits,
                fractional: shape.fractional_digits,
                signed: value.starts_with('-'),
                i64: Int64Type::parse(value).is_some(),
                u64: UInt64Type::parse(value).is_some(),
                decimal: value.contains('.'),
                fits_float: shape.significant_digits <= 15,
            };
        }
        if value.split_once(['e', 'E']).is_some_and(|(mantissa, _)| {
            decimal_shape(mantissa).is_some_and(|shape| {
                !shape.leading_zero
                    && shape.significant_digits <= 15
                    && Float64Type::parse(value).is_some_and(|number| {
                        number.is_finite() && (number != 0. || shape.significant_digits == 0)
                    })
            })
        }) {
            return Self::Float;
        }
        let bytes = value.as_bytes();
        if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
            return Self::Text;
        }
        if bytes.len() == 10 && Date32Type::parse(value).is_some() {
            return Self::Date;
        }
        if bytes
            .get(10)
            .is_some_and(|byte| matches!(byte, b'T' | b't' | b' '))
            && let Ok(timestamp) = string_to_datetime(&chrono::Utc, value)
        {
            let nanos = timestamp.timestamp_subsec_nanos() % 1_000 != 0;
            if !nanos || timestamp.timestamp_nanos_opt().is_some() {
                let zoned = value[11..].contains(['+', '-', 'Z', 'z']);
                return Self::DateTime { nanos, zoned };
            }
        }
        Self::Text
    }

    fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Unknown, next) => next,
            (Self::Boolean, Self::Boolean) => Self::Boolean,
            (Self::Date, Self::Date) => Self::Date,
            (
                Self::Float,
                Self::Float
                | Self::Number {
                    fits_float: true, ..
                },
            )
            | (
                Self::Number {
                    fits_float: true, ..
                },
                Self::Float,
            ) => Self::Float,
            (
                Self::DateTime { nanos, zoned },
                Self::DateTime {
                    nanos: next_nanos,
                    zoned: next_zoned,
                },
            ) if zoned == next_zoned => Self::DateTime {
                nanos: nanos || next_nanos,
                zoned,
            },
            (
                Self::Number {
                    integer,
                    fractional,
                    signed,
                    i64,
                    u64,
                    decimal,
                    fits_float,
                },
                Self::Number {
                    integer: ni,
                    fractional: nf,
                    signed: ns,
                    i64: si,
                    u64: ui,
                    decimal: nd,
                    fits_float: n_float,
                },
            ) => Self::Number {
                integer: integer.max(ni),
                fractional: fractional.max(nf),
                signed: signed || ns,
                i64: i64 && si,
                u64: u64 && ui,
                decimal: decimal || nd,
                fits_float: fits_float && n_float,
            },
            _ => Self::Text,
        }
    }

    fn data_type(self) -> DataType {
        match self {
            Self::Boolean => DataType::Boolean,
            Self::Date => DataType::Date32,
            Self::DateTime { nanos, zoned } => timestamp_type(nanos, zoned),
            Self::Float => DataType::Float64,
            Self::Number {
                i64: true,
                decimal: false,
                ..
            } => DataType::Int64,
            Self::Number {
                signed: false,
                u64: true,
                decimal: false,
                ..
            } => DataType::UInt64,
            Self::Number {
                integer,
                fractional,
                ..
            } => decimal_type(integer, fractional),
            Self::Unknown | Self::Text => DataType::Utf8View,
        }
    }
}

fn timestamp_type(nanos: bool, zoned: bool) -> DataType {
    let unit = if nanos {
        TimeUnit::Nanosecond
    } else {
        TimeUnit::Microsecond
    };
    DataType::Timestamp(unit, zoned.then(|| "UTC".into()))
}

fn decimal_type(integer: usize, fractional: usize) -> DataType {
    let required = integer.saturating_add(fractional);
    let precision = required.saturating_add(4);
    let scale = fractional as i8;
    // Include modest integer headroom without selecting Decimal256 for typical
    // amounts. More digits than the sampled scale are an error, never truncated.
    match required {
        0..=38 => DataType::Decimal128(precision.min(38) as u8, scale),
        39..=76 => DataType::Decimal256(precision.min(76) as u8, scale),
        _ => DataType::Utf8View,
    }
}

fn field_type(inferred: Inferred, options: &CsvOptions, index: usize) -> DataType {
    match options.column_types.get(&index) {
        Some(CsvColumnType::Text) => DataType::Utf8View,
        Some(CsvColumnType::Boolean) => DataType::Boolean,
        Some(CsvColumnType::Integer) => match inferred {
            Inferred::Number {
                i64: false,
                signed: false,
                u64: true,
                ..
            } => DataType::UInt64,
            _ => DataType::Int64,
        },
        Some(CsvColumnType::Decimal) => match inferred {
            Inferred::Number {
                integer,
                fractional,
                ..
            } if integer + fractional <= 76 => decimal_type(integer, fractional),
            _ => DataType::Decimal128(38, 18),
        },
        Some(CsvColumnType::Float) => DataType::Float64,
        Some(CsvColumnType::Date) => DataType::Date32,
        Some(CsvColumnType::DateTime) => match inferred {
            Inferred::DateTime { nanos, zoned } => timestamp_type(nanos, zoned),
            _ => timestamp_type(true, true),
        },
        None if options.infer_types => inferred.data_type(),
        None => DataType::Utf8View,
    }
}

#[derive(Clone)]
struct Anchor {
    row: usize,
    position: Position,
}

#[derive(Clone)]
struct Index {
    anchors: Vec<Anchor>,
    next: Anchor,
    complete: bool,
}

impl Index {
    fn new(position: Position) -> Self {
        let first = Anchor { row: 0, position };
        Self {
            anchors: vec![first.clone()],
            next: first,
            complete: false,
        }
    }

    fn advance(&mut self, position: &Position) -> Result<()> {
        self.next = Anchor {
            row: self
                .next
                .row
                .checked_add(1)
                .context("Text file row count exceeds addressable memory")?,
            position: position.clone(),
        };
        let last = self.anchors.last().unwrap();
        if self.next.row - last.row >= INDEX_ROWS
            || self.next.position.byte() - last.position.byte() >= INDEX_BYTES
        {
            self.anchors.push(self.next.clone());
        }
        Ok(())
    }

    fn jobs(&self, offset: usize, limit: usize) -> Vec<Job> {
        let end = offset.saturating_add(limit).min(self.next.row);
        if end <= offset {
            return Vec::new();
        }
        let first = self.anchors.partition_point(|anchor| anchor.row <= offset) - 1;
        let mut jobs = Vec::new();
        for (index, anchor) in self.anchors.iter().enumerate().skip(first) {
            if anchor.row >= end {
                break;
            }
            let next = self.anchors.get(index + 1).unwrap_or(&self.next);
            let start = offset.max(anchor.row);
            jobs.push(Job {
                byte: anchor.position.byte(),
                end_byte: next.position.byte(),
                source_rows: next.row - anchor.row,
                skip: start - anchor.row,
                start,
                rows: next.row.min(end) - start,
            });
        }
        jobs
    }
}

struct Sample {
    options: CsvOptions,
    schema: SchemaRef,
    index: Index,
    preview: Vec<Vec<String>>,
}

impl Sample {
    fn read(
        mut file: File,
        length: u64,
        options: Option<CsvOptions>,
        fallback: u8,
        purpose: SamplePurpose<'_>,
        cancel: &AtomicBool,
        on_bytes: impl Fn(u64, u64),
    ) -> Result<Self> {
        check_cancel(cancel)?;
        let mut options = match options {
            Some(options) => options,
            None => CsvOptions {
                delimiter: detect_delimiter(&mut file, fallback, cancel)?,
                ..CsvOptions::default()
            },
        };
        ensure!(
            valid_delimiter(options.delimiter),
            "Unsupported field separator"
        );
        ensure!(
            (1..=MAX_SAMPLE_RECORDS).contains(&options.sample_records),
            "The inference sample must be between 1 and 100,000 records."
        );
        let mut reader = parser(
            RecordReader::new(file),
            options.delimiter,
            options.has_header,
            false,
        );
        let header = reader
            .byte_headers()
            .context("Could not read the first text record")?;
        ensure!(!header.is_empty(), "The text file contains no fields.");
        let names = field_names(header, options.has_header)?;
        if let SamplePurpose::Reload(previous_types) = purpose {
            options.column_types = names
                .iter()
                .enumerate()
                .filter_map(|(index, name)| previous_types.get(name).map(|&kind| (index, kind)))
                .collect();
        }
        ensure!(
            options
                .column_types
                .keys()
                .all(|&index| index < names.len()),
            "A type override refers to a missing field."
        );
        let start = if options.has_header {
            reader.position().clone()
        } else {
            Position::new()
        };
        let mut index = Index::new(start);
        let mut scratch = RecordScratch::default();
        let mut inferred = vec![Inferred::Unknown; names.len()];
        let mut preview = Vec::new();
        let mut preview_budget = PREVIEW_BYTES;
        while index.next.row < options.sample_records {
            check_cancel(cancel)?;
            match read_record(&mut reader, &mut scratch.record, index.next.position.byte()) {
                Ok(true) => {}
                Ok(false) => {
                    index.complete = true;
                    index.next.position = reader.position().clone();
                    break;
                }
                // The browser can show the valid prefix. The scanner resumes
                // at that boundary and reports the precise failing record.
                Err(_) if matches!(purpose, SamplePurpose::Browse) && index.next.row > 0 => break,
                Err(error) => return Err(error).context("Could not read the text file sample"),
            }
            let preview_row = !matches!(purpose, SamplePurpose::Load | SamplePurpose::Browse)
                && preview.len() < PREVIEW_ROWS
                && preview_budget != 0;
            let mut displayed = Vec::new();
            for (col, raw) in scratch.record.iter().enumerate() {
                let value = std::str::from_utf8(raw).with_context(|| {
                    format!(
                        "Text file data must be valid UTF-8 (data record {}, field {})",
                        index.next.row + 1,
                        col + 1,
                    )
                })?;
                if options.infer_types || options.column_types.contains_key(&col) {
                    inferred[col].observe(value);
                }
                if preview_row {
                    let end = value
                        .char_indices()
                        .nth(PREVIEW_CHARS)
                        .map_or(value.len(), |(index, _)| index);
                    let mut end = end.min(preview_budget);
                    while !value.is_char_boundary(end) {
                        end -= 1;
                    }
                    preview_budget -= end;
                    displayed.push(value[..end].to_owned());
                }
            }
            if preview_row {
                preview.push(displayed);
            }
            scratch.trim();
            index.advance(reader.position())?;
            if matches!(purpose, SamplePurpose::Browse)
                && index.next.position.byte() >= BROWSE_SAMPLE_BYTES
            {
                break;
            }
            if index.next.row.is_multiple_of(1024) {
                on_bytes(index.next.position.byte(), length);
            }
        }
        if index.next.position.byte() >= length {
            index.complete = true;
        }
        on_bytes(index.next.position.byte(), length);
        let schema = Arc::new(Schema::new(
            names
                .into_iter()
                .zip(inferred)
                .enumerate()
                .map(|(index, (name, inferred))| {
                    Field::new(name, field_type(inferred, &options, index), true)
                })
                .collect::<Vec<_>>(),
        ));
        Ok(Self {
            options,
            schema,
            index,
            preview,
        })
    }
}

#[derive(Clone, Copy)]
struct Job {
    byte: u64,
    end_byte: u64,
    source_rows: usize,
    skip: usize,
    start: usize,
    rows: usize,
}

struct DecodePlan<'a> {
    projection: &'a [usize],
    schema: &'a SchemaRef,
    cancel: &'a AtomicBool,
}

pub(crate) struct CsvSource {
    pub info: DatasetInfo,
    pub options: CsvOptions,
    path: PathBuf,
    stamp: Stamp,
    schema: SchemaRef,
    decode_schema: SchemaRef,
    index: Mutex<Index>,
    browse_index: Mutex<Option<Arc<row_index::SharedIndex>>>,
    indexed_rows: AtomicUsize,
    count_known: AtomicBool,
    pool: rayon::ThreadPool,
}

impl CsvSource {
    pub(crate) fn open(
        path: &Path,
        options: Option<CsvOptions>,
        cancel: &AtomicBool,
        on_bytes: impl Fn(u64, u64) + Sync,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        let fallback = default_delimiter(path);
        let path = path
            .canonicalize()
            .with_context(|| format!("Could not open {}", path.display()))?;
        let file = File::open(&path)?;
        let stamp = Stamp::from_metadata(&file.metadata()?);
        let sample = Sample::read(
            file,
            stamp.len,
            options,
            fallback,
            SamplePurpose::Load,
            cancel,
            on_bytes,
        )?;
        Self::open_prepared(
            CsvPrepared {
                path,
                stamp,
                options: sample.options,
                schema: sample.schema,
                index: sample.index,
            },
            cancel,
        )
    }

    #[allow(dead_code)] // The standalone benchmarks open sources without an interactive preview.
    pub(crate) fn open_from_prepared(
        path: &Path,
        prepared: CsvPrepared,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        ensure!(
            path.canonicalize().ok().as_ref() == Some(&prepared.path),
            "Source changed after loading. Reload the file."
        );
        Self::open_prepared(prepared, cancel)
    }

    fn open_prepared(prepared: CsvPrepared, cancel: &AtomicBool) -> Result<Self> {
        let CsvPrepared {
            path,
            stamp,
            options,
            schema,
            index,
        } = prepared;
        check_cancel(cancel)?;
        stamp.validate_path(&path)?;
        crate::source_guard::register_sources(std::slice::from_ref(&path))?;
        let workers = std::thread::available_parallelism().map_or(1, |workers| workers.get());
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("csv-decode-{index}"))
            .build()?;
        let info = DatasetInfo {
            paths: vec![path.clone()],
            columns: columns_from_schema(&schema),
            row_count: index.next.row,
            row_count_known: index.complete,
            file_count: 1,
            file_size: stamp.len,
            ..Default::default()
        };
        let decode_schema = Arc::new(Schema::new(
            schema
                .fields()
                .iter()
                .map(|field| {
                    if matches!(
                        field.data_type(),
                        DataType::Decimal128(..)
                            | DataType::Decimal256(..)
                            | DataType::Timestamp(TimeUnit::Microsecond, _)
                    ) {
                        Field::new(field.name(), DataType::Utf8View, field.is_nullable())
                    } else {
                        field.as_ref().clone()
                    }
                })
                .collect::<Vec<_>>(),
        ));
        Ok(Self {
            path,
            stamp,
            schema,
            decode_schema,
            options,
            indexed_rows: AtomicUsize::new(info.row_count),
            count_known: AtomicBool::new(info.row_count_known),
            info,
            index: Mutex::new(index),
            browse_index: Mutex::default(),
            pool,
        })
    }

    pub(crate) fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    pub(crate) fn row_count(&self) -> (usize, bool) {
        if let Some(index) = self.browse_index.lock().unwrap().as_ref() {
            let progress = index.progress();
            return (progress.indexed_rows, progress.complete);
        }
        let known = self.count_known.load(Ordering::Acquire);
        (self.indexed_rows.load(Ordering::Acquire), known)
    }

    pub(crate) fn file_stamps(&self, limit: usize) -> SourceStamps {
        if limit == 0 {
            return Vec::new();
        }
        vec![(
            self.path.clone(),
            self.stamp.modified.map(|time| (time, self.stamp.len)),
        )]
    }

    pub(crate) fn pool(&self) -> &rayon::ThreadPool {
        &self.pool
    }

    pub(crate) fn validate_window(&self, offset: usize, limit: usize) -> Result<()> {
        let (rows, known) = self.row_count();
        if limit != 0 && (!known || offset < rows) {
            self.stamp.validate_path(&self.path)?;
        }
        Ok(())
    }

    fn selected_indices(&self, selected: &[String]) -> Result<Vec<usize>> {
        if selected.is_empty() {
            return Ok((0..self.schema.fields().len()).collect());
        }
        let mut seen = HashSet::with_capacity(selected.len());
        selected
            .iter()
            .map(|name| {
                ensure!(seen.insert(name), "Duplicate selected column: {name}");
                self.schema
                    .index_of(name)
                    .with_context(|| format!("Unknown column: {name}"))
            })
            .collect()
    }

    fn foreach_batch<T: Send>(
        &self,
        range: std::ops::Range<usize>,
        plan: &DecodePlan<'_>,
        on_bytes: impl Fn(u64, u64),
        process: impl Fn(RecordBatch, usize) -> Result<Option<T>> + Sync,
    ) -> Result<Vec<T>> {
        let browse_index = self.browse_index.lock().unwrap().clone();
        if let Some(index) = browse_index {
            return index.foreach_batch(self, range, plan, on_bytes, process);
        }
        let mut index = self.index.lock().unwrap_or_else(|error| error.into_inner());
        let (offset, end) = (range.start, range.end);
        let old_jobs = index.jobs(offset, end.saturating_sub(offset));
        let needs_scan = !index.complete && index.next.row < end;
        self.publish_count(&index);
        if needs_scan {
            on_bytes(index.next.position.byte(), self.stamp.len);
        }
        let mut output = self.decode_jobs(&old_jobs, None, plan, &process)?;
        if !needs_scan {
            return Ok(output);
        }

        let file = self.stamp.open(&self.path)?;
        let mut reader = parser(
            RecordReader::new(file),
            self.options.delimiter,
            self.options.has_header,
            true,
        );
        // Seed the already-consumed header so seeking never rereads it.
        reader.set_byte_headers(ByteRecord::new());
        let mut scratch = RecordScratch::default();
        reader.seek(index.next.position.clone())?;
        // A seek can make csv_core treat the new position as the start of a
        // file and strip a field's leading BOM, but positions still count the
        // BOM bytes. Arrow decoding below protects the original field bytes.
        let width = self.schema.fields().len();
        let mut reported = index.next.position.byte();
        while index.next.row < offset
            && Self::advance_index(
                self.stamp.len,
                &mut index,
                &mut reader,
                &mut scratch,
                width,
                plan.cancel,
            )?
        {
            if index.next.position.byte().saturating_sub(reported) >= 512 * 1024 {
                reported = index.next.position.byte();
                on_bytes(reported, self.stamp.len);
            }
        }
        // Uncaptured bytes can remain in csv::Reader's buffer. Back up once
        // before capturing requested data so every decoded byte is available.
        if !index.complete && index.next.row < end {
            reader.seek_raw(
                SeekFrom::Start(index.next.position.byte()),
                index.next.position.clone(),
            )?;
            reader.get_mut().captured = Some(Vec::with_capacity(self.capture_capacity(&index)));
        }
        while !index.complete && index.next.row < end {
            let start = index.next.clone();
            while index.next.row < end
                && index.next.position.byte() - start.position.byte() < READ_WAVE_BYTES
                && Self::advance_index(
                    self.stamp.len,
                    &mut index,
                    &mut reader,
                    &mut scratch,
                    width,
                    plan.cancel,
                )?
            {}
            let bytes = (index.next.position.byte() - start.position.byte()) as usize;
            if index.next.row > start.row {
                let mut jobs = index.jobs(start.row, index.next.row - start.row);
                // The first planned job may refer to an older sparse anchor
                // while this buffer starts exactly at the requested row.
                jobs[0].byte = start.position.byte();
                jobs[0].source_rows -= jobs[0].skip;
                jobs[0].skip = 0;
                let buffer = reader
                    .get_ref()
                    .captured
                    .as_deref()
                    .context("Missing text file block")?;
                output.extend(self.decode_jobs(
                    &jobs,
                    Some((
                        start.position.byte(),
                        buffer.get(..bytes).context("Incomplete text file block")?,
                    )),
                    plan,
                    &process,
                )?);
                if !index.complete && index.next.row < end {
                    reader
                        .get_mut()
                        .advance_capture(bytes, self.capture_capacity(&index));
                }
            }
            on_bytes(index.next.position.byte(), self.stamp.len);
        }
        self.stamp.validate_path(&self.path)?;
        self.publish_count(&index);
        on_bytes(index.next.position.byte(), self.stamp.len);
        Ok(output)
    }

    fn publish_count(&self, index: &Index) {
        self.indexed_rows.store(index.next.row, Ordering::Release);
        self.count_known.store(index.complete, Ordering::Release);
    }

    fn capture_capacity(&self, index: &Index) -> usize {
        self.stamp
            .len
            .saturating_sub(index.next.position.byte())
            .min(CAPTURE_BYTES as u64) as usize
    }

    fn advance_index(
        length: u64,
        index: &mut Index,
        reader: &mut Reader<RecordReader>,
        scratch: &mut RecordScratch,
        width: usize,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if index.complete {
            return Ok(false);
        }
        check_cancel(cancel)?;
        let record = &mut scratch.record;
        if !read_record(reader, record, index.next.position.byte())
            .context("Could not index the text file records")?
        {
            index.complete = true;
            index.next.position = reader.position().clone();
            return Ok(false);
        }
        ensure!(
            record.len() == width,
            "Text data record {} has {} fields, expected {width}",
            index.next.row + 1,
            record.len()
        );
        scratch.trim();
        index.advance(reader.position())?;
        if index.next.position.byte() >= length {
            index.complete = true;
        }
        Ok(true)
    }

    fn decode_jobs<T: Send>(
        &self,
        jobs: &[Job],
        captured: Option<(u64, &[u8])>,
        plan: &DecodePlan<'_>,
        process: &(impl Fn(RecordBatch, usize) -> Result<Option<T>> + Sync),
    ) -> Result<Vec<T>> {
        let chunks = self.pool.install(|| {
            jobs.par_iter()
                .map(|&job| {
                    let mut batches = Vec::new();
                    let mut visit = |batch, position| {
                        if let Some(value) = process(batch, position)? {
                            batches.push(value);
                        }
                        Ok(())
                    };
                    if let Some((base, buffer)) = captured {
                        let start = usize::try_from(
                            job.byte
                                .checked_sub(base)
                                .context("Invalid text file block offsets")?,
                        )?;
                        let end = usize::try_from(
                            job.end_byte
                                .checked_sub(base)
                                .context("Invalid text file block offsets")?,
                        )?;
                        let bytes = buffer
                            .get(start..end)
                            .context("Text record block is outside the scanned buffer")?;
                        // The whole indexed chunk is already available in memory. Feed
                        // Arrow the slice directly to avoid a second read buffer.
                        self.decode(job, bytes, plan, &mut visit)?;
                    } else {
                        self.decode_file_job(job, plan, visit)?;
                    }
                    Ok(batches)
                })
                .collect::<Result<Vec<_>>>()
        })?;
        Ok(chunks.into_iter().flatten().collect())
    }

    fn decode_file_job(
        &self,
        job: Job,
        plan: &DecodePlan<'_>,
        on_batch: impl FnMut(RecordBatch, usize) -> Result<()>,
    ) -> Result<()> {
        let mut file = self.stamp.open(&self.path)?;
        file.seek(SeekFrom::Start(job.byte))?;
        let input = BufReader::with_capacity(BUFFER_BYTES, file.take(job.end_byte - job.byte));
        self.decode(job, input, plan, on_batch)
    }

    fn decode(
        &self,
        job: Job,
        input: impl BufRead,
        plan: &DecodePlan<'_>,
        mut on_batch: impl FnMut(RecordBatch, usize) -> Result<()>,
    ) -> Result<()> {
        check_cancel(plan.cancel)?;
        // csv_core ignores an initial BOM. A synthetic blank line ensures a
        // literal U+FEFF in the first field of a later chunk survives unchanged.
        let prefix = if job.byte == 0 { &b""[..] } else { &b"\n"[..] };
        let input = Cursor::new(prefix).chain(input);
        // Arrow still frames and buffers every field before applying projection.
        // Aim for small decoder buffers even when rows contain long text.
        // A requested window may end before the next anchor. Estimate from
        // the whole byte span and its row count, not the requested subrange.
        let average_row_bytes = (job.end_byte - job.byte)
            .div_ceil(job.source_rows as u64)
            .max(1);
        let batch_rows =
            (DECODE_BATCH_BYTES / average_row_bytes).clamp(1, BATCH_ROWS as u64) as usize;
        let reader = ArrowCsvReader::new(self.decode_schema.clone())
            .with_delimiter(self.options.delimiter)
            .with_header(false)
            .with_projection(plan.projection.to_vec())
            .with_bounds(job.skip, job.skip + job.rows)
            .with_batch_size(batch_rows)
            .build_buffered(input)?;
        let mut position = job.start;
        for batch in reader {
            check_cancel(plan.cancel)?;
            let batch = batch.with_context(|| {
                format!(
                    "Could not decode text data records {} to {}. Check the column types.",
                    position + 1,
                    job.start + job.rows,
                )
            })?;
            let batch = restore_exact_types(batch, plan.schema, position)?;
            let rows = batch.num_rows();
            on_batch(batch, position)?;
            position += rows;
        }
        ensure!(
            position == job.start + job.rows,
            "The text decoder returned an incomplete row range."
        );
        check_cancel(plan.cancel)
    }

    pub(crate) fn load_window(
        &self,
        selected: &[String],
        offset: usize,
        limit: usize,
        resources: &Client,
        cancel: &AtomicBool,
        on_rows: impl Fn(usize) + Sync,
        on_index_bytes: impl Fn(u64, u64) + Sync,
    ) -> Result<ArrowTable> {
        check_cancel(cancel)?;
        ensure!(limit > 0, "Record count must be greater than zero.");
        self.validate_window(offset, limit)?;
        let projection = self.selected_indices(selected)?;
        let schema = Arc::new(self.schema.project(&projection)?);
        let loaded = AtomicUsize::new(0);
        let batches = self.foreach_batch(
            offset..offset.saturating_add(limit),
            &DecodePlan {
                projection: &projection,
                schema: &schema,
                cancel,
            },
            on_index_bytes,
            |batch, _| {
                let rows = batch.num_rows();
                on_rows(loaded.fetch_add(rows, Ordering::AcqRel) + rows);
                Ok(Some(batch))
            },
        )?;
        self.stamp.validate_path(&self.path)?;
        let table = ArrowTable::new_with_schema(columns_from_schema(&schema), batches, schema)?
            .with_source_row_start(offset.saturating_add(1));
        publish(table, resources, cancel)
    }

    pub(crate) fn load_filtered(
        &self,
        selected: &[String],
        predicate: Arc<dyn PhysicalExpr>,
        resources: &Client,
        cancel: &AtomicBool,
        on_rows: impl Fn(usize) + Sync,
        on_index_bytes: impl Fn(u64, u64) + Sync,
    ) -> Result<ArrowTable> {
        check_cancel(cancel)?;
        self.stamp.validate_path(&self.path)?;
        let selected = self.selected_indices(selected)?;
        let output = Arc::new(self.schema.project(&selected)?);
        let mut indices = selected.clone();
        for column in collect_columns(&predicate) {
            let index = self.schema.index_of(column.name())?;
            if !indices.contains(&index) {
                indices.push(index);
            }
        }
        let projection = Arc::new(self.schema.project(&indices)?);
        let predicate = reassign_expr_columns(predicate, &projection)?;
        ensure!(
            matches!(
                predicate.data_type(&projection)?,
                DataType::Boolean | DataType::Null
            ),
            "A source filter must return Boolean values"
        );
        let output_indices = (0..selected.len()).collect::<Vec<_>>();
        let examined = AtomicUsize::new(0);
        let chunks = self.foreach_batch(
            0..usize::MAX,
            &DecodePlan {
                projection: &indices,
                schema: &projection,
                cancel,
            },
            on_index_bytes,
            |batch, position| {
                let rows = batch.num_rows();
                let filtered = if let Some(mask) = predicate_mask(predicate.as_ref(), &batch)? {
                    match mask.true_count() {
                        0 => None,
                        count if count == rows => {
                            Some((batch.project(&output_indices)?, position, None))
                        }
                        _ => {
                            let positions = UInt64Array::from_iter_values(
                                mask.values()
                                    .set_indices()
                                    .map(|row| (position + row + 1) as u64),
                            );
                            let filtered =
                                filter_record_batch(&batch.project(&output_indices)?, &mask)?;
                            let filtered = compact_filtered_text(filtered)?;
                            Some((filtered, position, Some(positions)))
                        }
                    }
                } else {
                    None
                };
                on_rows(examined.fetch_add(rows, Ordering::AcqRel) + rows);
                Ok(filtered)
            },
        )?;
        self.stamp.validate_path(&self.path)?;
        let matched = chunks
            .iter()
            .map(|(batch, _, _)| batch.num_rows())
            .sum::<usize>();
        let complete = matched == self.row_count().0;
        let mut batches = Vec::new();
        let mut positions = Vec::new();
        for (batch, start, numbers) in chunks {
            check_cancel(cancel)?;
            if !complete {
                positions.push(numbers.unwrap_or_else(|| {
                    let first = start as u64 + 1;
                    UInt64Array::from_iter_values(first..first + batch.num_rows() as u64)
                }));
            }
            batches.push(batch);
        }
        let table = ArrowTable::new_with_schema(columns_from_schema(&output), batches, output)?;
        let table = if complete {
            table.with_source_row_start(1)
        } else {
            table.with_source_row_numbers(positions)?
        };
        publish(table, resources, cancel)
    }
}

fn publish(mut table: ArrowTable, resources: &Client, cancel: &AtomicBool) -> Result<ArrowTable> {
    let lease = resources.reserve_display(0)?;
    lease.grow_to(table.memory_size(), cancel)?;
    table.retain_memory(lease);
    check_cancel(cancel)?;
    Ok(table)
}

fn predicate_mask(
    predicate: &dyn PhysicalExpr,
    batch: &RecordBatch,
) -> Result<Option<BooleanArray>> {
    let result = predicate.evaluate(batch)?.into_array(batch.num_rows())?;
    if result.data_type() == &DataType::Null {
        return Ok(None);
    }
    let mask = result
        .as_any()
        .downcast_ref::<BooleanArray>()
        .context("A source filter must return Boolean values")?;
    ensure!(
        mask.len() == batch.num_rows(),
        "Source filter returned a different number of rows"
    );
    Ok(Some(if mask.null_count() == 0 {
        mask.clone()
    } else {
        prep_null_mask_filter(mask)
    }))
}

fn compact_filtered_text(batch: RecordBatch) -> Result<RecordBatch> {
    let (schema, mut columns, _) = batch.into_parts();
    for column in &mut columns {
        let Some(text) = column.as_any().downcast_ref::<StringViewArray>() else {
            continue;
        };
        // The Arrow filter preserves every payload block of a string view column,
        // including those containing only discarded rows. Copy only when the
        // surviving views use half or less and the saving is material.
        let bytes = text
            .data_buffers()
            .iter()
            .map(|buffer| buffer.len())
            .sum::<usize>();
        if bytes >= MIN_TEXT_BUFFER_SAVINGS {
            let used = text.total_buffer_bytes_used();
            if used <= bytes / 2 && bytes - used >= MIN_TEXT_BUFFER_SAVINGS {
                *column = Arc::new(text.gc());
            }
        }
    }
    Ok(RecordBatch::try_new(schema, columns)?)
}

fn canonical_decimal(value: &str) -> &str {
    if value.contains('.') {
        // Arrow's decimal parser accepts the original sign and trims leading
        // integer zeros. Drop only redundant trailing fractional zeros.
        let trimmed = value.trim_end_matches('0');
        if trimmed.ends_with('.') && trimmed.trim_start_matches(['-', '+']) == "." {
            "0"
        } else {
            trimmed
        }
    } else {
        value
    }
}

fn restore_exact_types(
    batch: RecordBatch,
    schema: &SchemaRef,
    start: usize,
) -> Result<RecordBatch> {
    if batch.schema_ref() == schema {
        return Ok(batch);
    }
    let (_, mut columns, _) = batch.into_parts();
    for (index, (column, field)) in columns.iter_mut().zip(schema.fields()).enumerate() {
        if let DataType::Timestamp(TimeUnit::Microsecond, timezone) = field.data_type() {
            let text = column
                .as_any()
                .downcast_ref::<StringViewArray>()
                .context("Invalid text file timestamp buffers")?;
            let values = text
                .iter()
                .enumerate()
                .map(|(row, value)| {
                    let Some(value) = value else { return Ok(None) };
                    let timestamp = string_to_datetime(&chrono::Utc, value).with_context(|| {
                        format!("Could not parse Timestamp at text data record {}, field {} ({})", start + row + 1, index + 1, field.name())
                    })?;
                    // Arrow's Timestamp parser keeps nine fractional digits. The
                    // first six are representable at this field's microsecond scale.
                    let bytes = value.as_bytes();
                    let excess_precision = bytes.get(19) == Some(&b'.')
                        && bytes.get(26..).is_some_and(|tail| {
                            tail.iter().take_while(|b| b.is_ascii_digit()).any(|&b| b != b'0')
                        });
                    ensure!(
                        timestamp.timestamp_subsec_nanos().is_multiple_of(1_000) && !excess_precision,
                        "Text data record {}, field {} ({}) has sub-microsecond Timestamp precision. Increase the inference sample or choose Text.",
                        start + row + 1, index + 1, field.name(),
                    );
                    Ok(Some(timestamp.timestamp_micros()))
                })
                .collect::<Result<TimestampMicrosecondArray>>()?;
            *column = Arc::new(values.with_timezone_opt(timezone.clone()));
            continue;
        }
        let (precision, scale) = match field.data_type() {
            DataType::Decimal128(precision, scale) | DataType::Decimal256(precision, scale) => {
                (*precision, *scale)
            }
            _ => continue,
        };
        let text = column
            .as_any()
            .downcast_ref::<StringViewArray>()
            .context("Invalid text file decimal buffers")?;
        let cells = text.iter().enumerate().map(|(row, value)| -> Result<Option<&str>> {
            let Some(value) = value else { return Ok(None) };
            let fits = decimal_shape(value).is_some_and(|shape| {
                shape.fractional_digits <= scale as usize
                    && shape.integer_digits + scale as usize <= precision as usize
            });
            ensure!(
                fits,
                "Text data record {}, field {} ({}) cannot fit Decimal({precision},{scale}) exactly. Increase the inference sample or choose Text.",
                start + row + 1, index + 1, field.name(),
            );
            Ok(Some(canonical_decimal(value)))
        });
        *column = if matches!(field.data_type(), DataType::Decimal128(..)) {
            let values = cells
                .map(|cell| {
                    cell?
                        .map(|v| parse_decimal::<Decimal128Type>(v, precision, scale))
                        .transpose()
                        .map_err(Into::into)
                })
                .collect::<Result<Decimal128Array>>()?;
            Arc::new(values.with_precision_and_scale(precision, scale)?) as ArrayRef
        } else {
            let values = cells
                .map(|cell| {
                    cell?
                        .map(|v| parse_decimal::<Decimal256Type>(v, precision, scale))
                        .transpose()
                        .map_err(Into::into)
                })
                .collect::<Result<Decimal256Array>>()?;
            Arc::new(values.with_precision_and_scale(precision, scale)?) as ArrayRef
        };
    }
    Ok(RecordBatch::try_new(schema.clone(), columns)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_import_prefers_expected_separators_for_ambiguous_suffixes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let cancel = AtomicBool::new(false);
        for (file, text, expected) in [
            ("input.TsV", "a\tb,c\n1\t2,3", b'\t'),
            ("input.tab", "a\tb,c\n1\t2,3", b'\t'),
            ("input.psv", "a|b,c\n1|2,3", b'|'),
            ("input.txt", "a|b\n1|2", b'|'),
            ("input.csv", "a|b,c\n1|2,3", b','),
            ("one.tsv", "a\n1\n", b'\t'),
        ] {
            let path = directory.path().join(file);
            std::fs::write(&path, text)?;
            let preview = CsvProbe::read_detected(&path, CsvOptions::default(), &cancel)?;
            assert_eq!(preview.options.delimiter, expected, "{file}");
            let source = CsvSource::open(&path, None, &cancel, |_, _| {})?;
            assert_eq!(source.options.delimiter, expected, "{file}");
            assert_eq!(source.row_count(), (1, true), "{file}");
        }
        Ok(())
    }

    #[test]
    fn text_import_comparison_can_detect_an_existing_custom_separator() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("revised.dat");
        let cancel = AtomicBool::new(false);
        std::fs::write(&path, "id^note\n1^\"uses, commas\"\n2^text\n")?;
        assert_eq!(detect_path_delimiter(&path, Some(b'^'), &cancel)?, b'^');
        std::fs::write(&path, "id\tnote\n1\ttext\n2\ttext\n")?;
        assert_eq!(detect_path_delimiter(&path, Some(b'^'), &cancel)?, b'\t');
        Ok(())
    }

    #[test]
    fn text_reload_matches_overrides_by_name_and_imports_the_revised_types() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("measurements.dat");
        let cancel = AtomicBool::new(false);
        let options = CsvOptions {
            delimiter: b'^',
            sample_records: 1,
            column_types: [(0, CsvColumnType::Integer), (2, CsvColumnType::Text)].into(),
            ..Default::default()
        };
        std::fs::write(&path, "removed^reading^id\n1^2^003\n")?;
        let original = CsvProbe::read(&path, Some(options.clone()), &cancel)?;
        let previous_types = options
            .column_types
            .iter()
            .map(|(&index, &kind)| (original.columns[index].name.clone(), kind))
            .collect();

        std::fs::write(&path, "id^reading\n004^2\n005^3\n")?;
        let refreshed = CsvProbe::read_for_reload(&path, options, &previous_types, &cancel)?;
        assert_eq!(refreshed.sampled_records, 1);
        assert_eq!(
            refreshed.options.column_types,
            [(0, CsvColumnType::Text)].into()
        );
        assert_eq!(refreshed.rows[0], ["004", "2"]);

        let mut revised = refreshed.options;
        revised.column_types.insert(0, CsvColumnType::Integer);
        let reviewed = CsvProbe::read(&path, Some(revised), &cancel)?;
        let source = CsvSource::open_from_prepared(&path, reviewed.prepared(), &cancel)?;
        let table = source.load_window(&[], 0, 2, &Client::new(), &cancel, |_| {}, |_, _| {})?;
        assert_eq!(table.display_text(0, 0), "4");
        assert_eq!(table.display_text(1, 0), "5");
        Ok(())
    }

    #[test]
    fn text_import_applies_custom_separator_to_preview_and_indexed_loads() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("input.dat");
        std::fs::write(
            &path,
            "id^note\n1^\"quoted^value\nline two\"\n2^éè\n3^last\n",
        )?;
        let cancel = AtomicBool::new(false);
        let options = CsvOptions {
            delimiter: b'^',
            sample_records: 1,
            ..Default::default()
        };
        let preview = CsvProbe::read(&path, Some(options.clone()), &cancel)?;
        assert_eq!(preview.columns[1].name, "note");
        assert_eq!(preview.rows[0][1], "quoted^value\nline two");
        let source = CsvSource::open(&path, Some(options.clone()), &cancel, |_, _| {})?;
        let resources = Client::new();
        let last = source.load_window(&[], 2, 1, &resources, &cancel, |_| {}, |_, _| {})?;
        assert_eq!(last.source_row_number(0), Some(3));
        assert_eq!(last.display_text(0, 1), "last");
        let first_two = source.load_window(&[], 0, 2, &resources, &cancel, |_| {}, |_, _| {})?;
        assert_eq!(first_two.display_text(0, 1), "quoted^value\nline two");
        assert_eq!(first_two.display_text(1, 1), "éè");
        let invalid = CsvOptions {
            delimiter: b'"',
            ..options
        };
        assert!(CsvProbe::read(&path, Some(invalid), &cancel).is_err());
        Ok(())
    }

    #[test]
    fn text_import_reuses_preview_schema_and_rejects_replaced_sources() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("preview.txt");
        let cancel = AtomicBool::new(false);
        std::fs::write(&path, "value\n00012\n8\n")?;
        let probe = CsvProbe::read(&path, None, &cancel)?;
        let prepared = probe.prepared();
        let source = CsvSource::open_from_prepared(&path, prepared.clone(), &cancel)?;
        let table = source.load_window(&[], 0, 2, &Client::new(), &cancel, |_| {}, |_, _| {})?;
        assert_eq!(source.row_count(), (2, true));
        assert_eq!(table.display_text(0, 0), "00012");
        assert_eq!(table.schema().field(0).data_type(), &DataType::Utf8View);

        let other = directory.path().join("replacement.txt");
        std::fs::write(&other, "value\n1\n2\n")?;
        assert!(CsvSource::open_from_prepared(&other, prepared.clone(), &cancel).is_err());
        std::fs::rename(other, &path)?;
        assert!(CsvSource::open_from_prepared(&path, prepared, &cancel).is_err());
        Ok(())
    }

    #[test]
    fn text_import_does_not_truncate_microsecond_timestamp_columns() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("time.csv");
        std::fs::write(
            &path,
            "time,n\n2024-04-02T02:03:04.123456+01:00,1\n2024-04-02T01:03:04.123456000Z,2\n2024-04-02T01:03:04.123456789Z,3\n",
        )?;
        let cancel = AtomicBool::new(false);
        let options = CsvOptions {
            sample_records: 1,
            ..Default::default()
        };
        let source = CsvSource::open(&path, Some(options), &cancel, |_, _| {})?;
        assert_eq!(
            source.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        let table = source.load_window(&[], 0, 2, &Client::new(), &cancel, |_| {}, |_, _| {})?;
        assert_eq!(table.display_text(0, 0), table.display_text(1, 0));
        let error = source.load_window(&[], 2, 1, &Client::new(), &cancel, |_| {}, |_, _| {});
        assert!(format!("{:#}", error.unwrap_err()).contains("sub-microsecond"));
        Ok(())
    }

    #[test]
    fn csv_probe_and_random_ranges_use_logical_records_and_exact_arrow_types() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("multiline.csv");
        let mut text = String::from("\u{feff}id;ID;amount;date;note\r\n");
        let rows = INDEX_ROWS + 6;
        for row in 0..rows {
            let amount = "12345678901234567890123456789.123456789012345678";
            let note = match row {
                0 => "\"one\r\ntwo; \"\"quoted\"\"\"",
                INDEX_ROWS => "\u{feff}data BOM",
                _ => "ok",
            };
            text.push_str(&format!("{row};00{row};{amount};2024-03-01;{note}"));
            if row + 1 < rows {
                text.push_str("\r\n");
            }
        }
        std::fs::write(&path, text)?;
        let cancel = AtomicBool::new(false);
        let probe = CsvProbe::read(&path, None, &cancel)?;
        assert_eq!(probe.options.delimiter, b';');
        assert_eq!(probe.sampled_records, 1000);
        assert_eq!(probe.columns[1].name, "ID (2)");
        assert_eq!(probe.rows[0][4], "one\r\ntwo; \"quoted\"");
        let source = CsvSource::open(&path, Some(probe.options), &cancel, |_, _| {})?;
        assert_eq!(source.row_count(), (1000, false));
        assert_eq!(
            source.schema().field(2).data_type(),
            &DataType::Decimal256(51, 18)
        );
        assert_eq!(source.schema().field(3).data_type(), &DataType::Date32);
        let columns = ["note".into(), "id".into(), "amount".into(), "ID (2)".into()];
        let data = source.load_window(
            &columns,
            INDEX_ROWS - 1,
            4,
            &Client::new(),
            &cancel,
            |_| {},
            |_, _| {},
        )?;
        assert_eq!(data.row_count(), 4);
        assert_eq!(data.display_text(1, 0), "\u{feff}data BOM");
        assert_eq!(data.display_text(1, 1), INDEX_ROWS.to_string());
        assert_eq!(
            data.display_text(1, 2),
            "12345678901234567890123456789.123456789012345678"
        );
        assert_eq!(data.display_text(1, 3), format!("00{INDEX_ROWS}"));
        assert_eq!(data.source_row_number(1), Some(INDEX_ROWS + 1));
        let end = source.load_window(
            &columns,
            rows - 2,
            usize::MAX,
            &Client::new(),
            &cancel,
            |_| {},
            |_, _| {},
        )?;
        assert_eq!(end.row_count(), 2);
        assert_eq!(end.source_row_number(1), Some(rows));
        assert_eq!(source.row_count(), (rows, true));
        Ok(())
    }

    #[test]
    fn csv_headerless_text_checks_source_identity_and_does_not_truncate_late_decimals() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("amounts.csv");
        let cancel = AtomicBool::new(false);
        std::fs::write(&path, "\u{feff}1.23,,001\n1.234,2024-02-29,002\n")?;
        let options = CsvOptions {
            has_header: false,
            sample_records: 1,
            ..Default::default()
        };
        let source = CsvSource::open(&path, Some(options.clone()), &cancel, |_, _| {})?;
        assert_eq!(source.info.columns[0].name, "column_1");
        let decoded = source.load_window(&[], 0, 1, &Client::new(), &cancel, |_| {}, |_, _| {})?;
        assert_eq!(decoded.display_text(0, 0), "1.23");
        assert!(decoded.is_null(0, 1));
        assert_eq!(decoded.display_text(0, 2), "001");
        let error = source
            .load_window(
                &[],
                0,
                usize::MAX,
                &Client::new(),
                &cancel,
                |_| {},
                |_, _| {},
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot fit Decimal(7,2) exactly")
        );
        let source = CsvSource::open(
            &path,
            Some(CsvOptions {
                infer_types: false,
                ..options
            }),
            &cancel,
            |_, _| {},
        )?;
        let decoded = source.load_window(
            &[],
            0,
            usize::MAX,
            &Client::new(),
            &cancel,
            |_| {},
            |_, _| {},
        )?;
        assert_eq!(decoded.display_text(1, 0), "1.234");
        std::fs::rename(&path, directory.path().join("old.csv"))?;
        std::fs::write(&path, "\u{feff}1.23,,001\n1.234,2024-02-29,002\n")?;
        assert!(source.validate_window(0, 2).is_err());
        Ok(())
    }

    #[test]
    fn csv_scientific_inference_does_not_convert_long_exact_values_to_float() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("scientific.csv");
        let zeros = "0".repeat(300);
        std::fs::write(
            &path,
            format!("float,mixed,decimal\n1.2e-6,1.2e-6,1.23{zeros}\n2.0,9007199254740993,0.00\n"),
        )?;
        let cancel = AtomicBool::new(false);
        let source = CsvSource::open(&path, None, &cancel, |_, _| {})?;
        let schema = source.schema();
        assert_eq!(schema.field(0).data_type(), &DataType::Float64);
        assert_eq!(schema.field(1).data_type(), &DataType::Utf8View);
        assert_eq!(schema.field(2).data_type(), &DataType::Decimal128(7, 2));
        let data = source.load_window(
            &[],
            0,
            usize::MAX,
            &Client::new(),
            &cancel,
            |_| {},
            |_, _| {},
        )?;
        assert_eq!(data.display_text(1, 1), "9007199254740993");
        assert_eq!(data.display_text(0, 2), "1.23");
        Ok(())
    }

    #[test]
    fn csv_fresh_multiwave_load_and_indexed_seek_keep_same_original_rows() -> Result<()> {
        use std::io::{BufWriter, Write};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("wave.csv");
        let mut writer = BufWriter::new(File::create(&path)?);
        writeln!(writer, "id,payload")?;
        let raw = "x".repeat(4096);
        let rows = (READ_WAVE_BYTES as usize / raw.len()) + 2048;
        for row in 0..rows {
            if row.is_multiple_of(11) {
                writeln!(writer, "{row},\"first\n{raw}\"")?;
            } else {
                writeln!(writer, "{row},{raw}")?;
            }
        }
        writer.flush()?;
        drop(writer);
        let cancel = AtomicBool::new(false);
        let options = CsvOptions {
            sample_records: 7,
            ..Default::default()
        };
        let source = CsvSource::open(&path, Some(options), &cancel, |_, _| {})?;
        let calls = AtomicUsize::new(0);
        let columns = ["id".to_owned()];
        let loaded = source.load_window(
            &columns,
            2,
            usize::MAX,
            &Client::new(),
            &cancel,
            |_| {},
            |_, _| {
                calls.fetch_add(1, Ordering::Relaxed);
            },
        )?;
        assert!(calls.load(Ordering::Relaxed) >= 4);
        assert_eq!(loaded.row_count(), rows - 2);
        assert_eq!(loaded.display_text(0, 0), "2");
        assert_eq!(loaded.display_text(rows - 3, 0), (rows - 1).to_string());
        assert_eq!(source.row_count(), (rows, true));
        let seek = source.load_window(
            &columns,
            rows - 17,
            17,
            &Client::new(),
            &cancel,
            |_| {},
            |_, _| {
                panic!("an indexed seek must not rescan the source");
            },
        )?;
        assert_eq!(seek.source_row_number(0), Some(rows - 16));
        assert_eq!(seek.source_row_number(16), Some(rows));
        assert_eq!(seek.display_text(16, 0), (rows - 1).to_string());
        Ok(())
    }

    #[test]
    fn csv_releases_large_wave_storage_and_preserves_read_ahead() -> Result<()> {
        use std::io::Write;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("large-record.csv");
        let mut file = File::create(&path)?;
        file.write_all(b"id,payload\r\n0,first\r\n1,\"line one\r\n")?;
        io::copy(&mut io::repeat(b'x').take(CAPTURE_BYTES as u64), &mut file)?;
        file.write_all(b"\"\r\n2,after\r\n3,last")?;
        drop(file);

        let cancel = AtomicBool::new(false);
        let options = CsvOptions {
            sample_records: 1,
            ..Default::default()
        };
        let source = CsvSource::open(&path, Some(options), &cancel, |_, _| {})?;
        let columns = ["id".to_owned()];
        let loaded = source.load_window(
            &columns,
            1,
            usize::MAX,
            &Client::new(),
            &cancel,
            |_| {},
            |_, _| {},
        )?;
        assert_eq!(loaded.row_count(), 3);
        assert_eq!(loaded.source_row_number(1), Some(3));
        assert_eq!(loaded.display_text(1, 0), "2");
        assert_eq!(loaded.display_text(2, 0), "3");

        let mut reader = RecordReader::new(File::open(&path)?);
        let mut oversized = Vec::with_capacity(CAPTURE_BYTES + BUFFER_BYTES);
        oversized.extend_from_slice(b"readthen keep");
        reader.captured = Some(oversized);
        reader.advance_capture(4, BUFFER_BYTES);
        let captured = reader.captured.unwrap();
        assert_eq!(&captured, b"then keep");
        assert_eq!(captured.capacity(), BUFFER_BYTES);
        Ok(())
    }

    #[test]
    fn whole_text_filter_keeps_only_the_matching_string_payloads() -> Result<()> {
        use datafusion::physical_expr::expressions::col;
        use std::io::{BufWriter, Write};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sparse.csv");
        let mut writer = BufWriter::new(File::create(&path)?);
        writeln!(writer, "id,payload,keep")?;
        let payload = "test 中文".repeat(512);
        let rows = 4096;
        for row in 0..rows {
            writeln!(writer, "{row},{payload},{}", row % 128 == 0)?;
        }
        writer.flush()?;
        drop(writer);

        let cancel = AtomicBool::new(false);
        let source = CsvSource::open(&path, None, &cancel, |_, _| {})?;
        let selected = ["id".into(), "payload".into()];
        let keep = col("keep", &source.schema())?;
        let loaded =
            source.load_filtered(&selected, keep, &Client::new(), &cancel, |_| {}, |_, _| {})?;
        assert_eq!(loaded.row_count(), rows / 128);
        assert_eq!(loaded.source_row_number(1), Some(129));
        let last = loaded.row_count() - 1;
        assert_eq!(loaded.source_row_number(last), Some(rows - 127));
        assert_eq!(loaded.display_text(1, 0), "128");
        let retained_payload_bytes = loaded
            .raw_batches()
            .iter()
            .map(|batch| {
                let text = batch
                    .column(1)
                    .as_any()
                    .downcast_ref::<StringViewArray>()
                    .unwrap();
                assert!(text.iter().all(|value| value == Some(payload.as_str())));
                text.data_buffers()
                    .iter()
                    .map(|buffer| buffer.len())
                    .sum::<usize>()
            })
            .sum::<usize>();
        assert!(retained_payload_bytes < 2 * loaded.row_count() * payload.len());
        Ok(())
    }
}
