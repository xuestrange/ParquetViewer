//! Snapshot-only export. Arrow batches remain shared and views are materialized
//! one bounded batch at a time; source files are never queried or modified.
use crate::{
    columnar::{self, ArrowTable},
    engine::{ColumnInfo, ExportFormat},
};
use anyhow::{Context, Result, ensure};
use arrow::{array::Array, datatypes::SchemaRef};
use parquet::{arrow::ArrowWriter, basic::Compression, file::properties::WriterProperties};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_EXPORT: AtomicU64 = AtomicU64::new(0);
const BATCH_ROWS: usize = 8192;

struct Projection {
    indices: Vec<usize>,
    columns: Vec<ColumnInfo>,
    schema: SchemaRef,
    formatters: Vec<columnar::ExportFormatter>,
}
impl Projection {
    fn new(data: &ArrowTable, columns: &[ColumnInfo], selected: &[String]) -> Result<Self> {
        let schema = data.schema();
        ensure!(
            schema.fields().len() == columns.len(),
            "Arrow export schema differs from the snapshot"
        );
        ensure!(
            schema
                .fields()
                .iter()
                .zip(columns)
                .all(|(field, column)| field.name() == &column.name),
            "Arrow export field order differs from the snapshot"
        );
        let indices = if selected.is_empty() {
            (0..columns.len()).collect()
        } else {
            let mut seen = HashSet::new();
            selected
                .iter()
                .map(|name| {
                    ensure!(seen.insert(name), "Duplicate export column: {name}");
                    columns
                        .iter()
                        .position(|column| &column.name == name)
                        .with_context(|| {
                            format!(
                                "Export column is not present in the displayed snapshot: {name}"
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?
        };
        let schema = Arc::new(schema.project(&indices)?);
        let columns: Vec<ColumnInfo> = indices
            .iter()
            .map(|&index| columns[index].clone())
            .collect();
        let formatters = columns
            .iter()
            .map(|column| columnar::ExportFormatter::new(&column.data_type))
            .collect();
        Ok(Self {
            indices,
            columns,
            schema,
            formatters,
        })
    }
    fn table(&self, data: &ArrowTable) -> Result<ArrowTable> {
        data.project_columns(&self.indices)
    }
}

/// Export the immutable displayed Arrow snapshot in the requested field order.
/// Failed/cancelled writes leave an existing destination intact. The staged file
/// is opened with create_new, synchronized, and atomically renamed only after
/// source protection and cancellation have been checked again.
pub fn export_snapshot(
    data: &ArrowTable,
    columns: &[ColumnInfo],
    selected: &[String],
    sources: &[PathBuf],
    path: &Path,
    format: ExportFormat,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    crate::source_guard::guard_destination(path, sources)?;
    let projection = Projection::new(data, columns, selected)?;
    // Project before materializing indexed rows, so unselected wide/Binary
    // columns never enter Arrow interleave or any format writer.
    let data = projection.table(data)?;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute
        .parent()
        .context("Export destination has no parent folder")?
        .canonicalize()
        .context("Export destination folder does not exist")?;
    let target = parent.join(
        absolute
            .file_name()
            .context("Export destination needs a file name")?,
    );
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = parent.join(format!(
        ".parquetviewer-{}-{nonce}-{}.tmp",
        std::process::id(),
        NEXT_EXPORT.fetch_add(1, Ordering::Relaxed)
    ));
    crate::source_guard::guard_destination(&temporary, sources)?;
    let mut file = OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&temporary)?;
    let outcome = (|| {
        match format {
            ExportFormat::Csv => write_csv(&data, &projection, &mut file, cancel)?,
            ExportFormat::Parquet => write_parquet(&data, &projection, &mut file, cancel)?,
            ExportFormat::Json => write_json(&data, &projection, &mut file, cancel)?,
            ExportFormat::Xlsx => write_xlsx(&data, &projection, &mut file, cancel)?,
            ExportFormat::Xls => write_xls(&data, &projection, &mut file, cancel)?,
        }
        file.sync_all()?;
        check_cancel(cancel)?;
        crate::source_guard::guard_destination(&target, sources)?;
        fs::rename(&temporary, &target).context("Could not save the exported file")?;
        Ok(())
    })();
    drop(file);
    if outcome.is_err() && crate::source_guard::guard_destination(&temporary, sources).is_ok() {
        let _ = fs::remove_file(&temporary);
    }
    outcome
}

fn write_parquet(
    data: &ArrowTable,
    projection: &Projection,
    file: &mut File,
    cancel: &AtomicBool,
) -> Result<()> {
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .set_max_row_group_row_count(Some(65_536))
        .build();
    let options = crate::parquet_compat::writer_options(&projection.schema, properties)?;
    let mut writer = ArrowWriter::try_new_with_options(file, projection.schema.clone(), options)?;
    for batch in data.export_batches(BATCH_ROWS, cancel) {
        check_cancel(cancel)?;
        writer.write(&batch?)?;
        if writer.memory_size() >= 16 * 1024 * 1024 {
            writer.flush()?;
        }
    }
    check_cancel(cancel)?;
    writer.close()?;
    Ok(())
}

fn write_json(
    data: &ArrowTable,
    projection: &Projection,
    file: &mut File,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut writer = BufWriter::new(file);
    writer.write_all(b"[")?;
    let mut first = true;
    for batch in data.export_batches(BATCH_ROWS, cancel) {
        let batch = batch?;
        for row in 0..batch.num_rows() {
            check_cancel(cancel)?;
            if !first {
                writer.write_all(b",\n")?;
            }
            first = false;
            writer.write_all(b"{")?;
            for (index, column) in projection.columns.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",")?;
                }
                serde_json::to_writer(&mut writer, &column.name)?;
                writer.write_all(b":")?;
                // One explicit value at a time retains decimal precision and
                // the application's lossless nested/Binary representation.
                let value =
                    projection.formatters[index].json_value(batch.column(index).as_ref(), row);
                serde_json::to_writer(&mut writer, &value)?;
            }
            writer.write_all(b"}")?;
        }
    }
    writer.write_all(b"]\n")?;
    writer.flush()?;
    Ok(())
}

fn text_value(value: Value) -> String {
    match value {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

fn write_csv(
    data: &ArrowTable,
    projection: &Projection,
    file: &mut File,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(file);
    writer.write_record(projection.columns.iter().map(|column| &column.name))?;
    for batch in data.export_batches(BATCH_ROWS, cancel) {
        let batch = batch?;
        for row in 0..batch.num_rows() {
            check_cancel(cancel)?;
            for (index, formatter) in projection.formatters.iter().enumerate() {
                match formatter.json_value(batch.column(index).as_ref(), row) {
                    Value::Null => writer.write_field("")?,
                    value => writer.write_field(text_value(value))?,
                }
            }
            writer.write_record(std::iter::empty::<&str>())?;
        }
    }
    writer.flush()?;
    Ok(())
}
fn excel_number(array: &dyn Array, text: &str) -> Option<f64> {
    // Excel's numeric cells preserve only 15 decimal digits. All larger exact
    // integers/decimals stay text; dates and numeric-looking strings stay text.
    (array.data_type().is_numeric() && text.chars().filter(char::is_ascii_digit).count() <= 15)
        .then(|| text.parse::<f64>().ok().filter(|value| value.is_finite()))
        .flatten()
}
fn check_excel_text(text: &str) -> Result<()> {
    ensure!(
        text.encode_utf16().count() <= 32_767,
        "Excel text cells support at most 32,767 UTF-16 units. Choose a format without Excel's cell-length limit."
    );
    Ok(())
}
fn write_xlsx(
    data: &ArrowTable,
    projection: &Projection,
    file: &mut File,
    cancel: &AtomicBool,
) -> Result<()> {
    use rust_xlsxwriter::Workbook;
    ensure!(
        projection.columns.len() <= 16_384,
        "Excel supports at most 16,384 columns."
    );
    ensure!(
        data.row_count() < 1_048_576,
        "Excel supports at most 1,048,575 data rows plus the header."
    );
    let mut workbook = Workbook::new();
    // Flushed rows use temporary storage instead of retaining the entire table.
    let sheet = workbook.add_worksheet_with_constant_memory();
    sheet.set_name("Parquet data")?;
    sheet.set_freeze_panes(1, 0)?;
    for (index, column) in projection.columns.iter().enumerate() {
        check_excel_text(&column.name)?;
        sheet.write_string(0, index as u16, &column.name)?;
        sheet.set_column_width(index as u16, 22.)?;
    }
    let mut row_index = 1u32;
    for batch in data.export_batches(BATCH_ROWS, cancel) {
        let batch = batch?;
        for row in 0..batch.num_rows() {
            check_cancel(cancel)?;
            for index in 0..projection.columns.len() {
                let array = batch.column(index).as_ref();
                let value = projection.formatters[index].json_value(array, row);
                if value.is_null() {
                    continue;
                }
                let col = index as u16;
                if let Value::Bool(value) = value {
                    sheet.write_boolean(row_index, col, value)?;
                } else {
                    let text = text_value(value);
                    if let Some(number) = excel_number(array, &text) {
                        sheet.write_number(row_index, col, number)?;
                    } else {
                        check_excel_text(&text)?;
                        sheet.write_string(row_index, col, &text)?;
                    }
                }
            }
            row_index += 1;
        }
    }
    check_cancel(cancel)?;
    // write_string deliberately never interprets untrusted '=...' as formulas.
    workbook.save_to_writer(file)?;
    Ok(())
}

/// Unicode BIFF8 with a shared-string table and OLE/DIFAT sector chains.
fn write_xls(
    data: &ArrowTable,
    projection: &Projection,
    file: &mut File,
    cancel: &AtomicBool,
) -> Result<()> {
    ensure!(
        projection.columns.len() <= 256,
        "Legacy XLS supports at most 256 columns. Choose XLSX instead."
    );
    ensure!(
        data.row_count() < 65_536,
        "Legacy XLS supports at most 65,535 data rows plus the header. Choose XLSX instead."
    );
    let mut sheet = Vec::new();
    let mut strings = BiffStrings::default();
    biff_bof(&mut sheet, 0x0010)?;
    let mut dimensions = Vec::new();
    dimensions.extend_from_slice(&0u32.to_le_bytes());
    dimensions.extend_from_slice(&(data.row_count() as u32 + 1).to_le_bytes());
    dimensions.extend_from_slice(&0u16.to_le_bytes());
    dimensions.extend_from_slice(&(projection.columns.len() as u16).to_le_bytes());
    dimensions.extend_from_slice(&0u16.to_le_bytes());
    biff_record(&mut sheet, 0x0200, &dimensions)?;
    for (index, column) in projection.columns.iter().enumerate() {
        biff_label(&mut sheet, &mut strings, 0, index as u16, &column.name)?;
    }
    let mut row_index = 1u16;
    for batch in data.export_batches(BATCH_ROWS, cancel) {
        let batch = batch?;
        for row in 0..batch.num_rows() {
            check_cancel(cancel)?;
            for index in 0..projection.columns.len() {
                let array = batch.column(index).as_ref();
                let value = projection.formatters[index].json_value(array, row);
                if value.is_null() {
                    continue;
                }
                let mut payload = Vec::from(row_index.to_le_bytes());
                payload.extend_from_slice(&(index as u16).to_le_bytes());
                payload.extend_from_slice(&0u16.to_le_bytes());
                if let Value::Bool(value) = value {
                    payload.extend_from_slice(&[u8::from(value), 0]);
                    biff_record(&mut sheet, 0x0205, &payload)?;
                } else {
                    let text = text_value(value);
                    if let Some(number) = excel_number(array, &text) {
                        payload.extend_from_slice(&number.to_le_bytes());
                        biff_record(&mut sheet, 0x0203, &payload)?;
                    } else {
                        biff_label(&mut sheet, &mut strings, row_index, index as u16, &text)?;
                    }
                }
            }
            row_index = row_index.saturating_add(1);
        }
    }
    biff_record(&mut sheet, 0x000a, &[])?;
    let mut workbook = Vec::new();
    biff_bof(&mut workbook, 0x0005)?;
    biff_record(&mut workbook, 0x0042, &1200u16.to_le_bytes())?;
    let mut font = Vec::new();
    for value in [200u16, 0, 0x7fff, 400, 0] {
        font.extend_from_slice(&value.to_le_bytes());
    }
    font.extend_from_slice(&[0, 0, 0, 0, 5, 0]);
    font.extend_from_slice(b"Arial");
    biff_record(&mut workbook, 0x0031, &font)?;
    biff_record(&mut workbook, 0x00e0, &[0; 20])?;
    let mut boundsheet = Vec::from(0u32.to_le_bytes());
    boundsheet.extend_from_slice(&[0, 0, 12, 1]);
    for unit in "Parquet data".encode_utf16() {
        boundsheet.extend_from_slice(&unit.to_le_bytes());
    }
    let boundsheet_offset = workbook.len() + 4;
    biff_record(&mut workbook, 0x0085, &boundsheet)?;
    biff_sst(&mut workbook, &strings, cancel)?;
    drop(strings);
    biff_record(&mut workbook, 0x000a, &[])?;
    let offset = u32::try_from(workbook.len()).context(
        "Legacy XLS workbook offsets exceed the format's 32-bit range. Choose XLSX instead.",
    )?;
    workbook[boundsheet_offset..boundsheet_offset + 4].copy_from_slice(&offset.to_le_bytes());
    workbook.extend_from_slice(&sheet);
    drop(sheet);
    check_cancel(cancel)?;
    let mut writer = BufWriter::new(file);
    ole_workbook(workbook, cancel, &mut writer)?;
    writer.flush()?;
    Ok(())
}

fn biff_record(output: &mut Vec<u8>, kind: u16, payload: &[u8]) -> Result<()> {
    ensure!(
        payload.len() <= 8224,
        "Legacy XLS record is too large. Choose XLSX instead."
    );
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    output.extend_from_slice(payload);
    Ok(())
}

fn biff_bof(output: &mut Vec<u8>, stream_type: u16) -> Result<()> {
    let mut payload = Vec::new();
    for value in [0x0600u16, stream_type, 0x0dbb, 0x07cc] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    payload.extend_from_slice(&0x41u32.to_le_bytes());
    payload.extend_from_slice(&0x06u32.to_le_bytes());
    biff_record(output, 0x0809, &payload)
}

#[derive(Default)]
struct BiffStrings {
    indices: HashMap<Arc<str>, u32>,
    values: Vec<(Arc<str>, u16)>,
    references: u32,
}

impl BiffStrings {
    fn intern(&mut self, text: &str) -> Result<u32> {
        self.references = self
            .references
            .checked_add(1)
            .filter(|count| *count <= i32::MAX as u32)
            .context(
                "Legacy XLS shared-string reference count exceeds its signed 32-bit format limit.",
            )?;
        if let Some(&index) = self.indices.get(text) {
            return Ok(index);
        }
        let length = text.encode_utf16().count();
        ensure!(
            length <= 32_767,
            "Legacy XLS text cells support at most 32,767 UTF-16 units. Choose a format without Excel's cell-length limit."
        );
        let index =
            u32::try_from(self.values.len()).context("Legacy XLS has too many unique strings.")?;
        let text: Arc<str> = Arc::from(text);
        self.values.push((text.clone(), length as u16));
        self.indices.insert(text, index);
        Ok(index)
    }
}

fn biff_label(
    output: &mut Vec<u8>,
    strings: &mut BiffStrings,
    row: u16,
    column: u16,
    text: &str,
) -> Result<()> {
    let mut payload = Vec::new();
    for value in [row, column, 0] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    payload.extend_from_slice(&strings.intern(text)?.to_le_bytes());
    biff_record(output, 0x00fd, &payload) // LabelSst; MS-XLS 2.4.149.
}

/// MS-XLS 2.4.265 / 2.5.293: fixed string headers stay in one record;
/// continuation of character bytes begins with a fresh fHighByte flag.
fn biff_sst(output: &mut Vec<u8>, strings: &BiffStrings, cancel: &AtomicBool) -> Result<()> {
    let mut kind = 0x00fc;
    let mut payload = Vec::with_capacity(8224);
    payload.extend_from_slice(&strings.references.to_le_bytes());
    payload.extend_from_slice(&(strings.values.len() as u32).to_le_bytes());
    for (text, length) in &strings.values {
        check_cancel(cancel)?;
        if payload.len() + 3 > 8224 {
            biff_record(output, kind, &payload)?;
            payload.clear();
            kind = 0x003c;
        }
        payload.extend_from_slice(&length.to_le_bytes());
        payload.push(1); // UTF-16, no rich text or phonetic data.
        for ch in text.chars() {
            let mut units = [0; 2];
            let units = ch.encode_utf16(&mut units);
            if payload.len() + units.len() * 2 > 8224 {
                check_cancel(cancel)?;
                biff_record(output, kind, &payload)?;
                payload.clear();
                payload.push(1); // Continuing this string, still UTF-16.
                kind = 0x003c;
            }
            // Keeping surrogate pairs together also supports decoders that
            // independently decode each continuation record's UTF-16 slice.
            for unit in units {
                payload.extend_from_slice(&unit.to_le_bytes());
            }
        }
    }
    biff_record(output, kind, &payload)
}

/// CFB v3 writer with one regular stream and chained DIFAT sectors.
/// Layout follows Microsoft's MS-CFB header/FAT/directory specifications.
fn ole_workbook(mut workbook: Vec<u8>, cancel: &AtomicBool, writer: &mut impl Write) -> Result<()> {
    const FREE: u32 = 0xffff_ffff;
    const END: u32 = 0xffff_fffe;
    const FAT: u32 = 0xffff_fffd;
    const DIFAT: u32 = 0xffff_fffc;
    check_cancel(cancel)?;
    ensure!(
        workbook.len() < 0x8000_0000,
        "Legacy XLS exceeds the CFB v3 2 GiB file limit. Choose XLSX instead."
    );
    // Streams below 4096 use MiniFAT. Padding a BIFF stream after EOF is valid,
    // and lets this single-stream exporter use regular FAT sectors consistently.
    workbook.resize(workbook.len().max(4096).div_ceil(512) * 512, 0);
    let sectors = workbook.len() / 512;
    let mut fat_sectors = (sectors + 1).div_ceil(128);
    let mut difat_sectors = fat_sectors.saturating_sub(109).div_ceil(127);
    loop {
        let next_fat = (sectors + 1 + fat_sectors + difat_sectors).div_ceil(128);
        let next_difat = next_fat.saturating_sub(109).div_ceil(127);
        if next_fat == fat_sectors && next_difat == difat_sectors {
            break;
        }
        fat_sectors = next_fat;
        difat_sectors = next_difat;
    }
    let file_size = (1 + sectors + 1 + fat_sectors + difat_sectors) * 512;
    ensure!(
        file_size <= 0x8000_0000,
        "Legacy XLS exceeds the CFB v3 2 GiB file limit. Choose XLSX instead."
    );
    let first_fat = sectors + 1;
    let first_difat = first_fat + fat_sectors;
    let mut header = vec![0u8; 512];
    header[..8].copy_from_slice(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]);
    header[24..26].copy_from_slice(&0x003eu16.to_le_bytes());
    header[26..28].copy_from_slice(&3u16.to_le_bytes());
    header[28..30].copy_from_slice(&0xfffeu16.to_le_bytes());
    header[30..32].copy_from_slice(&9u16.to_le_bytes());
    header[32..34].copy_from_slice(&6u16.to_le_bytes());
    header[44..48].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
    header[48..52].copy_from_slice(&(sectors as u32).to_le_bytes());
    header[56..60].copy_from_slice(&4096u32.to_le_bytes());
    header[60..64].copy_from_slice(&END.to_le_bytes());
    header[68..72].copy_from_slice(
        &(if difat_sectors == 0 {
            END
        } else {
            first_difat as u32
        })
        .to_le_bytes(),
    );
    header[72..76].copy_from_slice(&(difat_sectors as u32).to_le_bytes());
    for index in 0..109 {
        let sector = if index < fat_sectors {
            (first_fat + index) as u32
        } else {
            FREE
        };
        header[76 + index * 4..80 + index * 4].copy_from_slice(&sector.to_le_bytes());
    }
    fn directory_entry(name: &str, kind: u8, child: u32, start: u32, size: u64) -> Vec<u8> {
        let mut entry = vec![0u8; 128];
        let name = name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        for (index, unit) in name.iter().enumerate() {
            entry[index * 2..index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        entry[64..66].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        entry[66] = kind;
        entry[67] = 1; // Black red-black tree node.
        entry[68..72].copy_from_slice(&FREE.to_le_bytes());
        entry[72..76].copy_from_slice(&FREE.to_le_bytes());
        entry[76..80].copy_from_slice(&child.to_le_bytes());
        entry[116..120].copy_from_slice(&start.to_le_bytes());
        entry[120..128].copy_from_slice(&size.to_le_bytes());
        entry
    }
    let mut directory = directory_entry("Root Entry", 5, 1, END, 0);
    directory.extend_from_slice(&directory_entry(
        "Workbook",
        2,
        FREE,
        0,
        workbook.len() as u64,
    ));
    directory.resize(512, 0);
    let mut fat = vec![FREE; fat_sectors * 128];
    for (index, entry) in fat.iter_mut().enumerate().take(sectors) {
        *entry = if index + 1 == sectors {
            END
        } else {
            index as u32 + 1
        };
    }
    fat[sectors] = END;
    for entry in fat.iter_mut().skip(sectors + 1).take(fat_sectors) {
        *entry = FAT;
    }
    for entry in fat.iter_mut().skip(first_difat).take(difat_sectors) {
        *entry = DIFAT;
    }
    check_cancel(cancel)?;
    // Stream the compound-file envelope. Its sector layout does not require a
    // second workbook-sized allocation around the already-built BIFF stream.
    writer.write_all(&header)?;
    writer.write_all(&workbook)?;
    drop(workbook);
    writer.write_all(&directory)?;
    for chunk in fat.chunks(128) {
        check_cancel(cancel)?;
        for entry in chunk {
            writer.write_all(&entry.to_le_bytes())?;
        }
    }
    for index in 0..difat_sectors {
        check_cancel(cancel)?;
        for slot in 0..127 {
            let fat_index = 109 + index * 127 + slot;
            let sector = if fat_index < fat_sectors {
                (first_fat + fat_index) as u32
            } else {
                FREE
            };
            writer.write_all(&sector.to_le_bytes())?;
        }
        let next = if index + 1 == difat_sectors {
            END
        } else {
            (first_difat + index + 1) as u32
        };
        writer.write_all(&next.to_le_bytes())?;
    }
    Ok(())
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "Operation cancelled.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::{
        array::*,
        buffer::NullBuffer,
        datatypes::{DataType, Field, Schema},
        record_batch::RecordBatch,
    };
    use calamine::{Data, Reader, Xls, Xlsx, open_workbook};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn table(values: Vec<(&str, &str, ArrayRef)>) -> Result<(ArrowTable, Vec<ColumnInfo>)> {
        let columns = values
            .iter()
            .map(|(name, kind, _)| ColumnInfo {
                name: (*name).into(),
                data_type: (*kind).into(),
                nullable: true,
            })
            .collect::<Vec<_>>();
        let batch =
            RecordBatch::try_from_iter(values.into_iter().map(|(name, _, array)| (name, array)))?;
        Ok((ArrowTable::new(columns.clone(), vec![batch])?, columns))
    }
    fn export(
        data: &ArrowTable,
        columns: &[ColumnInfo],
        path: &Path,
        format: ExportFormat,
    ) -> Result<()> {
        export_snapshot(
            data,
            columns,
            &[],
            &[],
            path,
            format,
            &AtomicBool::new(false),
        )
    }
    fn values(path: &Path) -> Result<Vec<RecordBatch>> {
        Ok(ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?
            .build()?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    #[test]
    fn csv_preserves_utf8_quotes_line_breaks_precision_and_nested_values() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut list = ListBuilder::new(StringBuilder::new());
        list.values().append_value("值,\"字\"");
        list.values().append_null();
        list.append(true);
        list.append_null();
        let (data, columns) = table(vec![
            (
                "标题,\"带引号\"",
                "VARCHAR",
                Arc::new(StringArray::from(vec![Some("中文,\"😀\"\r\n换行"), None])),
            ),
            (
                "amount",
                "DECIMAL(38,18)",
                Arc::new(
                    Decimal128Array::from(vec![
                        Some(12345678901234567890123456789012345678i128),
                        None,
                    ])
                    .with_precision_and_scale(38, 18)?,
                ),
            ),
            ("items", "VARCHAR[]", Arc::new(list.finish())),
            (
                "id",
                "UBIGINT",
                Arc::new(UInt64Array::from(vec![u64::MAX, 0])),
            ),
        ])?;
        let view = data.with_row_indices(UInt64Array::from(vec![1, 0]))?;
        let path = dir.path().join("csv.csv");
        export_snapshot(
            &view,
            &columns,
            &["items", "id", "标题,\"带引号\"", "amount"].map(str::to_owned),
            &[],
            &path,
            ExportFormat::Csv,
            &AtomicBool::new(false),
        )?;
        let raw = fs::read_to_string(&path)?;
        assert!(raw.starts_with("items,id,\"标题,\"\"带引号\"\"\",amount\r\n"));
        let mut reader = csv::Reader::from_reader(raw.as_bytes());
        let rows = reader
            .records()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["", "0", "", ""]);
        assert_eq!(
            rows[1],
            vec![
                r#"["值,\"字\"",null]"#,
                "18446744073709551615",
                "中文,\"😀\"\r\n换行",
                "12345678901234567890.123456789012345678",
            ]
        );
        Ok(())
    }

    #[test]
    fn csv_keeps_empty_result_header_and_single_column_null_rows() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (data, columns) = table(vec![(
            "missing",
            "VARCHAR",
            Arc::new(StringArray::from(vec![None, Some(""), Some("NULL")])),
        )])?;
        let path = dir.path().join("one.csv");
        export(&data, &columns, &path, ExportFormat::Csv)?;
        let mut reader = csv::Reader::from_path(&path)?;
        let rows = reader
            .records()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec![""]);
        assert_eq!(rows[1], vec![""]);
        assert_eq!(rows[2], vec!["NULL"]);
        let empty = data.with_row_indices(UInt64Array::from(vec![] as Vec<u64>))?;
        export(&empty, &columns, &path, ExportFormat::Csv)?;
        assert_eq!(fs::read_to_string(&path)?, "missing\r\n");
        Ok(())
    }

    #[test]
    fn json_and_parquet_preserve_exact_numbers_nested_values_and_field_order() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut items = ListBuilder::new(Int64Builder::new());
        items.values().append_value(7);
        items.values().append_null();
        items.append(true);
        let details = StructArray::from(vec![(
            Arc::new(Field::new("value", DataType::Int64, true)),
            Arc::new(Int64Array::from(vec![Some(9)])) as ArrayRef,
        )]);
        let mut mapping = MapBuilder::new(None, StringBuilder::new(), Int64Builder::new());
        mapping.keys().append_value("中文");
        mapping.values().append_value(42);
        mapping.append(true)?;
        let (data, columns) = table(vec![
            ("id", "UBIGINT", Arc::new(UInt64Array::from(vec![u64::MAX]))),
            (
                "amount",
                "DECIMAL(38,18)",
                Arc::new(
                    Decimal128Array::from(vec![12345678901234567890123456789012345678i128])
                        .with_precision_and_scale(38, 18)?,
                ),
            ),
            ("missing", "BIGINT", Arc::new(Int64Array::from(vec![None]))),
            (
                "blob",
                "BLOB",
                Arc::new(BinaryArray::from(vec![Some(&b"\0A"[..])])),
            ),
            ("items", "BIGINT[]", Arc::new(items.finish())),
            ("details", "STRUCT(value BIGINT)", Arc::new(details)),
            ("mapping", "MAP(VARCHAR,BIGINT)", Arc::new(mapping.finish())),
        ])?;
        let selected = [
            "amount", "id", "items", "blob", "missing", "details", "mapping",
        ]
        .map(str::to_owned);
        let json = dir.path().join("values.json");
        export_snapshot(
            &data,
            &columns,
            &selected,
            &[],
            &json,
            ExportFormat::Json,
            &AtomicBool::new(false),
        )?;
        let text = fs::read_to_string(&json)?;
        assert!(text.find("\"amount\"").unwrap() < text.find("\"id\"").unwrap());
        assert!(text.contains("12345678901234567890.123456789012345678"));
        assert!(text.contains("18446744073709551615"));
        let parsed: Value = serde_json::from_str(&text)?;
        assert_eq!(parsed[0]["id"].to_string(), u64::MAX.to_string());
        assert!(parsed[0]["missing"].is_null());
        assert_eq!(parsed[0]["items"], serde_json::json!([7, null]));
        assert_eq!(parsed[0]["details"], serde_json::json!({"value":9}));
        assert_eq!(parsed[0]["mapping"], serde_json::json!({"中文":42}));
        assert_eq!(parsed[0]["blob"], Value::String("\\x00A".into()));
        let parquet = dir.path().join("values.parquet");
        export_snapshot(
            &data,
            &columns,
            &selected,
            &[],
            &parquet,
            ExportFormat::Parquet,
            &AtomicBool::new(false),
        )?;
        let batches = values(&parquet)?;
        assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 1);
        assert_eq!(
            batches[0]
                .schema()
                .fields()
                .iter()
                .map(|f| f.name())
                .collect::<Vec<_>>(),
            selected.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            batches[0].column(0).data_type(),
            &DataType::Decimal128(38, 18)
        );
        assert_eq!(
            batches[0]
                .column(1)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
                .value(0),
            u64::MAX
        );
        assert_eq!(
            batches[0]
                .column(3)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .unwrap()
                .value(0),
            b"\0A"
        );
        Ok(())
    }

    #[test]
    fn indexed_snapshot_export_keeps_sort_order_across_original_batch_boundaries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let columns = vec![ColumnInfo {
            name: "id".into(),
            data_type: "BIGINT".into(),
            nullable: false,
        }];
        let batches = [0..8192, 8192..16384, 16384..16390]
            .into_iter()
            .map(|range| {
                RecordBatch::try_from_iter([(
                    "id",
                    Arc::new(Int64Array::from_iter_values(range.map(i64::from))) as ArrayRef,
                )])
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let base = ArrowTable::new(columns.clone(), batches)?;
        let indices = vec![16389u64, 0, 8192, 8191, 16384];
        let view = base.with_row_indices(UInt64Array::from(indices.clone()))?;
        for (format, name) in [
            (ExportFormat::Parquet, "ordered.parquet"),
            (ExportFormat::Json, "ordered.json"),
        ] {
            let path = dir.path().join(name);
            export(&view, &columns, &path, format)?;
            let actual = if format == ExportFormat::Json {
                serde_json::from_slice::<Vec<Value>>(&fs::read(path)?)?
                    .into_iter()
                    .map(|row| row["id"].as_u64().unwrap())
                    .collect::<Vec<_>>()
            } else {
                values(&path)?
                    .iter()
                    .flat_map(|batch| {
                        batch
                            .column(0)
                            .as_any()
                            .downcast_ref::<Int64Array>()
                            .unwrap()
                            .values()
                            .iter()
                            .map(|&id| id as u64)
                    })
                    .collect()
            };
            assert_eq!(actual, indices);
        }
        assert_eq!(base.row_count(), 16390);
        Ok(())
    }

    #[test]
    fn selected_indexed_export_projects_before_gather_for_all_formats() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let first_amount = 12345678901234567890123456789012345678i128;
        let amounts = Arc::new(
            Decimal128Array::from(vec![
                Some(first_amount),
                None,
                Some(first_amount + 2),
                Some(first_amount + 3),
            ])
            .with_precision_and_scale(38, 18)?,
        ) as ArrayRef;
        let nested = Arc::new(StructArray::try_new(
            vec![
                Arc::new(Field::new("amount", amounts.data_type().clone(), true)),
                Arc::new(Field::new("flag", DataType::Boolean, true)),
            ]
            .into(),
            vec![
                amounts.clone(),
                Arc::new(BooleanArray::from(vec![
                    Some(true),
                    None,
                    Some(false),
                    Some(true),
                ])),
            ],
            Some(NullBuffer::from(vec![true, false, true, true])),
        )?) as ArrayRef;
        let hidden = Arc::new(BinaryArray::from(vec![Some(&vec![9u8; 262_144][..]); 4]));
        let interval_field = Field::new("span", DataType::FixedSizeBinary(12), true).with_metadata(
            [(
                "parquetviewer.logical_type".into(),
                "INTERVAL_MONTH_DAY_MILLISECOND_LE".into(),
            )]
            .into_iter()
            .collect(),
        );
        let intervals = Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            [
                Some(&[1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..]),
                None,
                Some(&[0u8, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0][..]),
                Some(&[0u8, 0, 0, 0, 30, 0, 0, 0, 0, 0, 0, 0][..]),
            ]
            .into_iter(),
            12,
        )?) as ArrayRef;
        let schema = Arc::new(Schema::new_with_metadata(
            vec![
                Field::new("id", DataType::Int64, false),
                Field::new("hidden", DataType::Binary, true),
                Field::new("record", nested.data_type().clone(), true)
                    .with_metadata([("note".into(), "preserved".into())].into_iter().collect()),
                interval_field,
                Field::new("amount", amounts.data_type().clone(), true),
            ],
            [("fixture".into(), "project-first".into())]
                .into_iter()
                .collect(),
        ));
        let columns = crate::arrow_types::columns_from_schema(&schema);
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![10, 11, 12, 13])),
                hidden.clone(),
                nested,
                intervals,
                amounts,
            ],
        )?;
        let base = ArrowTable::new(columns.clone(), vec![batch.slice(0, 2), batch.slice(2, 2)])?
            .with_source_row_start(101);
        let hidden_observer = Arc::downgrade(base.raw_batches()[0].column(1));
        drop(batch);
        drop(hidden);
        let order = vec![3u64, 1, 0, 2, 3];
        let view = base.with_row_indices(UInt64Array::from(order.clone()))?;
        let selected = ["record", "id", "span", "amount"].map(str::to_owned);
        let projection = Projection::new(&view, &columns, &selected)?;
        let prepared = projection.table(&view)?;
        assert_eq!(prepared.schema(), projection.schema);
        assert!(
            prepared
                .raw_batches()
                .iter()
                .all(|batch| batch.num_columns() == 4)
        );
        assert!(
            prepared.memory_size() < 16_384,
            "hidden payload must be absent before Arrow interleave"
        );
        assert_eq!(
            (0..5)
                .map(|row| prepared.source_row_number(row))
                .collect::<Vec<_>>(),
            [Some(104), Some(102), Some(101), Some(103), Some(104)]
        );

        let amount_text = [
            Some("12345678901234567890.123456789012345678"),
            None,
            Some("12345678901234567890.123456789012345680"),
            Some("12345678901234567890.123456789012345681"),
        ];
        let span_text = [Some("1 month"), None, Some("00:00:00.256"), Some("30 days")];
        let expected = order.iter().map(|&row| {
            let row = row as usize;
            let amount = amount_text[row].map_or(Value::Null, |text| Value::Number(text.parse().unwrap()));
            let record = if row == 1 { Value::Null } else { serde_json::json!({"amount": amount, "flag":row != 2}) };
            serde_json::json!({"record":record,"id":row+10,"span":span_text[row],"amount":amount})
        }).collect::<Vec<_>>();

        for (format, name) in [
            (ExportFormat::Csv, "selected.csv"),
            (ExportFormat::Json, "selected.json"),
            (ExportFormat::Parquet, "selected.parquet"),
            (ExportFormat::Xlsx, "selected.xlsx"),
            (ExportFormat::Xls, "selected.xls"),
        ] {
            let path = dir.path().join(name);
            export_snapshot(
                &view,
                &columns,
                &selected,
                &[],
                &path,
                format,
                &AtomicBool::new(false),
            )?;
            match format {
                ExportFormat::Csv => {
                    let mut reader = csv::Reader::from_path(&path)?;
                    assert!(
                        reader
                            .headers()?
                            .iter()
                            .eq(selected.iter().map(String::as_str))
                    );
                    let rows = reader
                        .records()
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    assert_eq!(rows.len(), expected.len());
                    for (row, expected) in rows.iter().zip(&expected) {
                        for (index, name) in selected.iter().enumerate() {
                            let value = expected[name].clone();
                            assert_eq!(
                                row.get(index),
                                Some(
                                    if value.is_null() {
                                        String::new()
                                    } else {
                                        text_value(value)
                                    }
                                    .as_str()
                                )
                            );
                        }
                    }
                }
                ExportFormat::Json => assert_eq!(
                    serde_json::from_reader::<_, Vec<Value>>(File::open(&path)?)?,
                    expected
                ),
                ExportFormat::Parquet => {
                    let file = File::open(&path)?;
                    let metadata = crate::parquet_compat::load_metadata(&file)?;
                    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, metadata);
                    assert_eq!(reader.schema().metadata(), schema.metadata());
                    assert_eq!(
                        reader
                            .schema()
                            .field(0)
                            .metadata()
                            .get("note")
                            .map(String::as_str),
                        Some("preserved")
                    );
                    let batches = reader
                        .build()?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    let reloaded = ArrowTable::new(projection.columns.clone(), batches)?;
                    for (row, expected) in expected.iter().enumerate() {
                        assert_eq!(reloaded.cell(row, 1).text, expected["id"].to_string());
                        if expected["record"].is_null() {
                            assert!(reloaded.is_null(row, 0));
                            assert!(reloaded.is_null(row, 2));
                            assert!(reloaded.is_null(row, 3));
                        } else {
                            assert_eq!(
                                reloaded.cell(row, 0).json.as_ref(),
                                Some(&expected["record"])
                            );
                            assert_eq!(
                                reloaded.cell(row, 2).text,
                                expected["span"].as_str().unwrap()
                            );
                            assert_eq!(reloaded.cell(row, 3).text, expected["amount"].to_string());
                        }
                    }
                }
                ExportFormat::Xls | ExportFormat::Xlsx => {
                    let range = if matches!(format, ExportFormat::Xls) {
                        let mut workbook: Xls<_> = open_workbook(&path)?;
                        workbook.worksheet_range("Parquet data")?
                    } else {
                        let mut workbook: Xlsx<_> = open_workbook(&path)?;
                        workbook.worksheet_range("Parquet data")?
                    };
                    for (column, name) in selected.iter().enumerate() {
                        assert_eq!(range.get((0, column)), Some(&Data::String(name.clone())));
                    }
                    for (row, expected) in expected.iter().enumerate() {
                        assert_eq!(
                            range.get((row + 1, 1)),
                            Some(&Data::Float(expected["id"].as_f64().unwrap()))
                        );
                        for (column, name) in [(0, "record"), (2, "span"), (3, "amount")] {
                            let value = &expected[name];
                            let expected = if value.is_null() {
                                Data::Empty
                            } else {
                                Data::String(
                                    value
                                        .as_str()
                                        .map_or_else(|| value.to_string(), str::to_owned),
                                )
                            };
                            assert_eq!(range.get((row + 1, column)), Some(&expected));
                        }
                    }
                }
            }
        }
        drop(base);
        drop(view);
        assert!(
            hidden_observer.upgrade().is_none(),
            "prepared export must retain selected ArrayRefs only"
        );
        assert_eq!(prepared.materialize_range(0..5)?.num_columns(), 4);
        Ok(())
    }

    #[test]
    fn empty_typed_exports_keep_schema_and_failures_never_truncate_destinations() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let columns = vec![ColumnInfo {
            name: "id".into(),
            data_type: "BIGINT".into(),
            nullable: true,
        }];
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let data = ArrowTable::new(columns.clone(), vec![RecordBatch::new_empty(schema)])?;
        let parquet = dir.path().join("empty.parquet");
        export(&data, &columns, &parquet, ExportFormat::Parquet)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(parquet)?)?;
        assert_eq!(reader.metadata().file_metadata().num_rows(), 0);
        assert_eq!(reader.schema().field(0).data_type(), &DataType::Int64);
        let json = dir.path().join("empty.json");
        export(&data, &columns, &json, ExportFormat::Json)?;
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(json)?)?,
            serde_json::json!([])
        );
        let destination = dir.path().join("existing");
        fs::write(&destination, b"keep")?;
        for format in [
            ExportFormat::Csv,
            ExportFormat::Json,
            ExportFormat::Parquet,
            ExportFormat::Xls,
            ExportFormat::Xlsx,
        ] {
            assert!(
                export_snapshot(
                    &data,
                    &columns,
                    &[],
                    &[],
                    &destination,
                    format,
                    &AtomicBool::new(true)
                )
                .is_err()
            );
            assert_eq!(fs::read(&destination)?, b"keep");
        }
        assert!(
            export_snapshot(
                &data,
                &columns,
                &["id".into(), "id".into()],
                &[],
                &destination,
                ExportFormat::Json,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(
            export_snapshot(
                &data,
                &columns,
                &["missing".into()],
                &[],
                &destination,
                ExportFormat::Json,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert_eq!(fs::read(destination)?, b"keep");
        Ok(())
    }

    #[test]
    fn exports_cannot_replace_sources_or_their_hard_link_aliases() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source.parquet");
        let alias = dir.path().join("alias.parquet");
        fs::write(&source, b"source remains read-only")?;
        fs::hard_link(&source, &alias)?;
        let (data, columns) = table(vec![("id", "BIGINT", Arc::new(Int64Array::from(vec![1])))])?;
        for destination in [&source, &alias] {
            assert!(
                export_snapshot(
                    &data,
                    &columns,
                    &[],
                    std::slice::from_ref(&source),
                    destination,
                    ExportFormat::Parquet,
                    &AtomicBool::new(false)
                )
                .is_err()
            );
        }
        assert_eq!(fs::read(source)?, b"source remains read-only");
        Ok(())
    }

    #[test]
    fn unicode_excel_roundtrip_preserves_large_integers_booleans_and_formula_text() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (data, columns) = table(vec![
            ("id", "BIGINT", Arc::new(Int64Array::from(vec![42]))),
            (
                "phrase",
                "VARCHAR",
                Arc::new(StringArray::from(vec!["中文😀 =1+1"])),
            ),
            ("yes", "BOOLEAN", Arc::new(BooleanArray::from(vec![true]))),
            (
                "exact_integer",
                "BIGINT",
                Arc::new(Int64Array::from(vec![9007199254740993])),
            ),
            (
                "formula_text",
                "VARCHAR",
                Arc::new(StringArray::from(vec!["=1+1"])),
            ),
        ])?;
        let expected = [
            Data::Float(42.),
            Data::String("中文😀 =1+1".into()),
            Data::Bool(true),
            Data::String("9007199254740993".into()),
            Data::String("=1+1".into()),
        ];
        let xls = dir.path().join("unicode.xls");
        export(&data, &columns, &xls, ExportFormat::Xls)?;
        let mut book: Xls<_> = open_workbook(&xls)?;
        let range = book.worksheet_range("Parquet data")?;
        for (column, value) in expected.iter().enumerate() {
            assert_eq!(range.get((1, column)), Some(value));
        }
        let xlsx = dir.path().join("unicode.xlsx");
        export(&data, &columns, &xlsx, ExportFormat::Xlsx)?;
        let mut book: Xlsx<_> = open_workbook(&xlsx)?;
        let range = book.worksheet_range("Parquet data")?;
        for (column, value) in expected.iter().enumerate() {
            assert_eq!(range.get((1, column)), Some(value));
        }
        assert!(
            book.worksheet_formula("Parquet data")?
                .cells()
                .all(|(_, _, formula)| formula.is_empty())
        );
        Ok(())
    }

    #[test]
    fn excel_cell_limits_preserve_existing_files_and_clean_staging_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let too_long = "😀".repeat(16_384);
        let (data, columns) = table(vec![(
            "payload",
            "VARCHAR",
            Arc::new(StringArray::from(vec![too_long.as_str()])),
        )])?;
        for (format, name) in [
            (ExportFormat::Xls, "existing.xls"),
            (ExportFormat::Xlsx, "existing.xlsx"),
        ] {
            let target = dir.path().join(name);
            fs::write(&target, b"keep existing file")?;
            assert!(export(&data, &columns, &target, format).is_err());
            assert_eq!(fs::read(&target)?, b"keep existing file");
        }
        assert!(fs::read_dir(dir.path())?.all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".parquetviewer-")
        }));
        let json = dir.path().join("retry.json");
        export(&data, &columns, &json, ExportFormat::Json)?;
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(json)?)?[0]["payload"].as_str(),
            Some(too_long.as_str())
        );
        Ok(())
    }

    #[test]
    fn legacy_xls_sst_continuations_preserve_maximum_unicode_text() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let maximum = format!("{}😀{}", "a".repeat(4097), "中".repeat(28_668));
        assert_eq!(maximum.encode_utf16().count(), 32_767);
        let expected = [
            maximum.clone(),
            "😀".repeat(2500),
            "after continuation".into(),
            maximum,
        ];
        let (data, columns) = table(vec![(
            "payload",
            "VARCHAR",
            Arc::new(StringArray::from(
                expected.iter().map(String::as_str).collect::<Vec<_>>(),
            )),
        )])?;
        let output = dir.path().join("long_text.xls");
        export(&data, &columns, &output, ExportFormat::Xls)?;
        let mut book: Xls<_> = open_workbook(output)?;
        let range = book.worksheet_range("Parquet data")?;
        assert_eq!(range.height(), expected.len() + 1);
        for (row, text) in expected.into_iter().enumerate() {
            assert_eq!(range.get((row + 1, 0)), Some(&Data::String(text)));
        }
        Ok(())
    }

    #[test]
    fn legacy_xls_large_workbook_chains_multiple_difat_sectors() -> Result<()> {
        use std::io::Read;
        let dir = tempfile::tempdir()?;
        let prefix = "中😀a".repeat(3000);
        let expected = (0..700).map(|i| format!("{prefix}{i}")).collect::<Vec<_>>();
        let (data, columns) = table(vec![
            (
                "id",
                "BIGINT",
                Arc::new(Int64Array::from_iter_values(0..700)),
            ),
            (
                "payload",
                "VARCHAR",
                Arc::new(StringArray::from(
                    expected.iter().map(String::as_str).collect::<Vec<_>>(),
                )),
            ),
        ])?;
        let output = dir.path().join("large.xls");
        export(&data, &columns, &output, ExportFormat::Xls)?;
        assert!(fs::metadata(&output)?.len() > 16 * 1024 * 1024);
        let mut header = [0; 512];
        File::open(&output)?.read_exact(&mut header)?;
        assert!(u32::from_le_bytes(header[44..48].try_into()?) > 109 + 127);
        assert!(u32::from_le_bytes(header[72..76].try_into()?) >= 2);
        let mut book: Xls<_> = open_workbook(output)?;
        let range = book.worksheet_range("Parquet data")?;
        assert_eq!((range.height(), range.width()), (701, 2));
        for (row, text) in expected.into_iter().enumerate() {
            assert_eq!(range.get((row + 1, 0)), Some(&Data::Float(row as f64)));
            assert_eq!(range.get((row + 1, 1)), Some(&Data::String(text)));
        }
        Ok(())
    }
}
