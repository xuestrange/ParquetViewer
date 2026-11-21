//! Independent, deterministic cases for the read/sort/export regression.
use anyhow::Result;
use arrow::{
    array::*,
    buffer::{NullBuffer, OffsetBuffer},
    datatypes::{DataType, Field},
    record_batch::RecordBatch,
};
use parquet::{
    arrow::ArrowWriter,
    basic::Compression,
    file::properties::{WriterProperties, WriterVersion},
};
use std::{fs, fs::OpenOptions, path::Path, sync::Arc};

fn batch(columns: Vec<(&str, ArrayRef)>) -> Result<RecordBatch> {
    Ok(RecordBatch::try_from_iter(columns)?)
}

fn write(path: &Path, name: &str, batch: RecordBatch, version: WriterVersion) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join(format!("{name}.parquet")))?;
    let properties = WriterProperties::builder()
        .set_writer_version(version)
        .set_compression(Compression::ZSTD(Default::default()))
        // Keep compression enabled for definition-level-only V2 pages.
        .set_data_page_v2_compression_ratio_threshold(100.0)
        .set_data_page_row_count_limit(8)
        .set_dictionary_enabled(false)
        .build();
    let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(properties))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

pub(super) fn generate(directory: &Path) -> Result<()> {
    fs::create_dir(directory)?;
    let list: ListArray = ListArray::from_iter_primitive::<arrow::datatypes::Int64Type, _, _>([
        Some(vec![Some(8), None, Some(-2)]),
        Some(vec![]),
        None,
    ]);
    let inner = StructArray::from(vec![
        (
            Arc::new(Field::new("value", DataType::Int64, true)),
            Arc::new(Int64Array::from(vec![Some(9), None])) as ArrayRef,
        ),
        (
            Arc::new(Field::new("note", DataType::Utf8, true)),
            Arc::new(StringArray::from(vec![Some("a"), Some("乙")])) as ArrayRef,
        ),
    ]);
    let outer = StructArray::from(vec![(
        Arc::new(Field::new("details", inner.data_type().clone(), true)),
        Arc::new(inner) as ArrayRef,
    )]);
    let nested = ListArray::new(
        Arc::new(Field::new("element", outer.data_type().clone(), true)),
        OffsetBuffer::from_lengths([2, 0, 0]),
        Arc::new(outer),
        Some(NullBuffer::from(vec![true, true, false])),
    );
    let mut map = MapBuilder::new(None, StringBuilder::new(), Int64Builder::new());
    map.keys().append_value("first");
    map.values().append_value(5);
    map.keys().append_value("missing");
    map.values().append_null();
    map.append(true)?;
    map.append(true)?;
    map.append(false)?;

    let amounts = Decimal128Array::from(vec![Some(-123_456_789_012_345_678), None, Some(37)])
        .with_precision_and_scale(30, 6)?;
    let bytes: &[u8] = &[0, 128, 255, 42];
    let cases = [
        (
            "dates_and_timestamps",
            batch(vec![
                (
                    "day",
                    Arc::new(Date32Array::from(vec![Some(-1), None, Some(20_000)])),
                ),
                (
                    "event",
                    Arc::new(
                        TimestampNanosecondArray::from(vec![
                            Some(-1),
                            Some(1_712_345_678_123_456_789),
                            None,
                        ])
                        .with_timezone("+05:30"),
                    ),
                ),
            ])?,
        ),
        (
            "decimals_and_booleans",
            batch(vec![
                ("amount", Arc::new(amounts)),
                (
                    "active",
                    Arc::new(BooleanArray::from(vec![Some(false), None, Some(true)])),
                ),
            ])?,
        ),
        (
            "binary_values",
            batch(vec![(
                "payload",
                Arc::new(BinaryArray::from(vec![Some(bytes), Some(&[]), None])),
            )])?,
        ),
        ("list_values", batch(vec![("items", Arc::new(list))])?),
        ("nested_structs", batch(vec![("items", Arc::new(nested))])?),
        (
            "map_values",
            batch(vec![("attributes", Arc::new(map.finish()))])?,
        ),
        (
            "field_name_case_collision",
            batch(vec![
                ("reading", Arc::new(Int64Array::from(vec![1, 2, 3]))),
                ("Reading", Arc::new(StringArray::from(vec!["a", "b", "c"]))),
                ("unrelated", Arc::new(Int64Array::from(vec![1, 4, 9]))),
            ])?,
        ),
    ];
    for (name, batch) in cases {
        write(directory, name, batch, WriterVersion::PARQUET_1_0)?;
    }
    write(
        directory,
        "all_null_data_pages_v2",
        batch(vec![
            ("missing", Arc::new(Float64Array::new_null(24))),
            (
                "value",
                Arc::new(Float64Array::from_iter_values((0..24).map(f64::from))),
            ),
        ])?,
        WriterVersion::PARQUET_2_0,
    )
}
