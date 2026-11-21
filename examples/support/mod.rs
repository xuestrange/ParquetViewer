//! Deterministic fixture generation using typed Arrow arrays and Parquet only.
#![allow(dead_code)]
use anyhow::{Context, Result, ensure};
use arrow::{
    array::*,
    buffer::{NullBuffer, OffsetBuffer, ScalarBuffer},
    datatypes::{Field, Fields},
    record_batch::RecordBatch,
};
use parquet::{
    arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder},
    basic::Compression,
    file::properties::WriterProperties,
};
use std::{
    fs::{self, File},
    path::Path,
    sync::Arc,
};

pub const LARGE_INTEGER_BASE: i128 = 90_071_992_547_409_931_234_567_890_123_456_789;
pub fn batch(columns: Vec<(&str, ArrayRef)>) -> Result<RecordBatch> {
    Ok(RecordBatch::try_from_iter(columns)?)
}
fn strings(values: impl IntoIterator<Item = Option<String>>) -> ArrayRef {
    Arc::new(StringArray::from_iter(values))
}
fn ints(values: impl IntoIterator<Item = Option<i64>>) -> ArrayRef {
    Arc::new(Int64Array::from_iter(values))
}
fn structure(columns: Vec<(&str, ArrayRef)>, valid: Option<Vec<bool>>) -> ArrayRef {
    let fields: Fields = columns
        .iter()
        .map(|(name, a)| Arc::new(Field::new(*name, a.data_type().clone(), true)))
        .collect();
    Arc::new(StructArray::new(
        fields,
        columns.into_iter().map(|(_, a)| a).collect(),
        valid.map(NullBuffer::from),
    ))
}
fn list(
    values: ArrayRef,
    lengths: impl IntoIterator<Item = usize>,
    valid: Option<Vec<bool>>,
) -> ArrayRef {
    let mut offsets = vec![0i32];
    for len in lengths {
        offsets.push(
            offsets
                .last()
                .copied()
                .unwrap()
                .checked_add(i32::try_from(len).unwrap())
                .unwrap(),
        );
    }
    Arc::new(ListArray::new(
        Arc::new(Field::new("item", values.data_type().clone(), true)),
        OffsetBuffer::new(ScalarBuffer::from(offsets)),
        values,
        valid.map(NullBuffer::from),
    ))
}
fn list_ints(values: impl IntoIterator<Item = Option<Vec<Option<i64>>>>) -> ArrayRef {
    let mut builder = ListBuilder::new(Int64Builder::new());
    for value in values {
        match value {
            Some(values) => {
                for value in values {
                    builder.values().append_option(value);
                }
                builder.append(true);
            }
            None => builder.append(false),
        }
    }
    Arc::new(builder.finish())
}
fn decimal(
    values: impl IntoIterator<Item = Option<i128>>,
    precision: u8,
    scale: i8,
) -> Result<ArrayRef> {
    Ok(Arc::new(
        Decimal128Array::from_iter(values).with_precision_and_scale(precision, scale)?,
    ))
}
fn day(text: &str) -> i32 {
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .unwrap()
        .signed_duration_since(chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
        .num_days() as i32
}
fn micros(text: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .unwrap()
        .and_utc()
        .timestamp_micros()
}
fn nanos(text: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .unwrap()
        .and_utc()
        .timestamp_nanos_opt()
        .unwrap()
}

/// Publish without replacing a preexisting file or symlink, even if another
/// process creates the requested destination while the fixture is being built.
pub fn write_fixture(
    path: &Path,
    total: usize,
    batch_rows: usize,
    compression: Compression,
    build: impl Fn(usize, usize) -> Result<RecordBatch>,
) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "Refusing to overwrite an existing fixture: {}",
        path.display()
    );
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let workspace = tempfile::Builder::new()
        .prefix(".arrow-fixture-")
        .tempdir_in(parent)?;
    let temporary = workspace.path().join("data.parquet");
    let first = build(0, total.min(batch_rows))?;
    let props = WriterProperties::builder()
        .set_compression(compression)
        .set_max_row_group_row_count(Some(32768))
        .build();
    let mut writer = ArrowWriter::try_new(File::create(&temporary)?, first.schema(), Some(props))?;
    if first.num_rows() > 0 {
        writer.write(&first)?;
    }
    for start in (first.num_rows()..total).step_by(batch_rows) {
        writer.write(&build(start, (total - start).min(batch_rows))?)?;
        if writer.memory_size() > 32 * 1024 * 1024 {
            writer.flush()?;
        }
    }
    writer.close()?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&temporary)?)?;
    ensure!(
        reader.metadata().file_metadata().num_rows() == total as i64,
        "Stored row count differs from the fixture"
    );
    fs::hard_link(&temporary, path).with_context(|| {
        format!(
            "Could not publish {} without replacing an existing destination",
            path.display()
        )
    })?;
    Ok(())
}
pub fn read_all(path: &Path) -> Result<Vec<RecordBatch>> {
    Ok(ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?
        .build()?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}
pub fn copy_prefix(source: &Path, destination: &Path, rows: usize) -> Result<()> {
    ensure!(
        fs::symlink_metadata(destination).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "Refusing to overwrite prefix destination"
    );
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let workspace = tempfile::Builder::new()
        .prefix(".arrow-prefix-")
        .tempdir_in(parent)?;
    let temporary = workspace.path().join("data.parquet");
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(source)?)?;
    let mut writer = ArrowWriter::try_new(
        File::create(&temporary)?,
        reader.schema().clone(),
        Some(
            WriterProperties::builder()
                .set_compression(Compression::UNCOMPRESSED)
                .set_max_row_group_row_count(Some(32768))
                .build(),
        ),
    )?;
    let mut left = rows;
    for batch in reader.with_batch_size(8192).build()? {
        let batch = batch?;
        if left == 0 {
            break;
        }
        let n = batch.num_rows().min(left);
        writer.write(&batch.slice(0, n))?;
        left -= n;
    }
    writer.close()?;
    fs::hard_link(temporary, destination)?;
    Ok(())
}

pub fn make_typed(path: &Path) -> Result<()> {
    write_fixture(
        path,
        60,
        8192,
        Compression::ZSTD(Default::default()),
        |start, n| {
            let ids = (start..start + n).map(|i| i as i64).collect::<Vec<_>>();
            batch(vec![
                ("id", Arc::new(Int64Array::from(ids.clone()))),
                (
                    "amount",
                    decimal(
                        ids.iter().map(|&i| {
                            Some(i as i128 * 1_000_000_000_000_000_000 + 123_456_789_012_345_678)
                        }),
                        38,
                        18,
                    )?,
                ),
                (
                    "event_date",
                    Arc::new(Date32Array::from(
                        ids.iter()
                            .map(|&i| day("2024-02-01") + i as i32)
                            .collect::<Vec<_>>(),
                    )),
                ),
                (
                    "happened_at",
                    Arc::new(TimestampNanosecondArray::from(
                        ids.iter()
                            .map(|&i| {
                                nanos("2024-02-01 12:34:56.123456789") + i * 86_400_000_000_000
                            })
                            .collect::<Vec<_>>(),
                    )),
                ),
                (
                    "local_time",
                    Arc::new(Time64MicrosecondArray::from(
                        ids.iter()
                            .map(|&i| (12 * 3600 + 34 * 60 + i % 60) * 1_000_000)
                            .collect::<Vec<_>>(),
                    )),
                ),
                (
                    "zoned_at",
                    Arc::new(
                        TimestampMicrosecondArray::from(vec![
                            micros("2024-02-01 04:34:56.123456");
                            n
                        ])
                        .with_timezone("UTC"),
                    ),
                ),
                (
                    "active",
                    Arc::new(BooleanArray::from(
                        ids.iter().map(|&i| i % 2 == 0).collect::<Vec<_>>(),
                    )),
                ),
                (
                    "title",
                    strings(
                        ids.iter()
                            .map(|&i| (i % 7 != 0).then(|| format!("Item {i}"))),
                    ),
                ),
                (
                    "items",
                    list_ints(ids.iter().map(|&i| Some(vec![Some(i), None, Some(i + 1)]))),
                ),
                (
                    "bytes",
                    Arc::new(BinaryArray::from_iter_values(
                        (0..n).map(|_| &b"\x01\x02"[..]),
                    )),
                ),
                (
                    "numeric_text",
                    strings(ids.iter().map(|i| Some(i.to_string()))),
                ),
            ])
        },
    )
}

pub fn make_nested(path: &Path) -> Result<()> {
    write_fixture(
        path,
        60,
        8192,
        Compression::ZSTD(Default::default()),
        |start, n| {
            let ids = (start..start + n).map(|i| i as i64).collect::<Vec<_>>();
            let mut tags = ListBuilder::new(StringBuilder::new());
            for &i in &ids {
                if i % 7 != 0 {
                    tags.values().append_value("Alpha");
                    tags.values().append_value(format!("tag {i}"));
                    tags.values().append_null();
                }
                tags.append(true);
            }
            let stats = structure(
                vec![
                    (
                        "score",
                        decimal(
                            ids.iter().map(|&i| {
                                Some(
                                    i as i128 * 1_000_000_000_000_000_000 + 123_456_789_012_345_678,
                                )
                            }),
                            38,
                            18,
                        )?,
                    ),
                    (
                        "active",
                        Arc::new(BooleanArray::from(
                            ids.iter().map(|i| i % 2 == 0).collect::<Vec<_>>(),
                        )),
                    ),
                ],
                None,
            );
            let profile = structure(
                vec![
                    ("age", ints(ids.iter().map(|i| Some(i + 20)))),
                    (
                        "name",
                        strings(ids.iter().map(|i| Some(format!("Person {i}")))),
                    ),
                    ("stats", stats),
                    ("tags", Arc::new(tags.finish())),
                    (
                        "day",
                        Arc::new(Date32Array::from(
                            ids.iter()
                                .map(|&i| day("2024-02-01") + i as i32)
                                .collect::<Vec<_>>(),
                        )),
                    ),
                ],
                Some(ids.iter().map(|i| i % 10 != 0).collect()),
            );
            let records = structure(
                vec![
                    ("code", ints(ids.iter().flat_map(|&i| [Some(i), None]))),
                    (
                        "label",
                        strings(ids.iter().flat_map(|i| [Some(format!("nested {i}")), None])),
                    ),
                ],
                Some((0..n).flat_map(|_| [true, false]).collect()),
            );
            let photo = structure(
                vec![
                    (
                        "bytes",
                        Arc::new(BinaryArray::from_iter_values((0..n).map(|_| {
                            &include_bytes!("../../packaging/assets/app-icon.svg")[..]
                        }))),
                    ),
                    ("path", strings((0..n).map(|_| Some("fixture.svg".into())))),
                ],
                None,
            );
            batch(vec![
                (
                    "id",
                    Arc::new(Int32Array::from(
                        ids.iter().map(|&i| i as i32).collect::<Vec<_>>(),
                    )),
                ),
                ("profile", profile),
                (
                    "numbers",
                    list_ints(ids.iter().map(|&i| {
                        if i % 10 == 0 {
                            None
                        } else if i % 7 == 0 {
                            Some(vec![])
                        } else {
                            Some(vec![Some(i), None, Some(i + 1)])
                        }
                    })),
                ),
                (
                    "flags",
                    list(
                        Arc::new(BooleanArray::from(
                            ids.iter()
                                .flat_map(|i| [Some(i % 2 == 0), None])
                                .collect::<Vec<_>>(),
                        )),
                        vec![2; n],
                        None,
                    ),
                ),
                (
                    "dates",
                    list(
                        Arc::new(Date32Array::from(
                            ids.iter()
                                .flat_map(|&i| [Some(day("2024-02-01") + i as i32), None])
                                .collect::<Vec<_>>(),
                        )),
                        vec![2; n],
                        None,
                    ),
                ),
                ("records", list(records, vec![2; n], None)),
                ("photo", photo),
            ])
        },
    )
}

pub fn make_null(path: &Path) -> Result<()> {
    write_fixture(
        path,
        100_000,
        8192,
        Compression::ZSTD(Default::default()),
        |start, n| {
            let ids = (start..start + n).map(|i| i as i64).collect::<Vec<_>>();
            let records = structure(
                vec![
                    (
                        "value",
                        ints(ids.iter().flat_map(|&i| [Some(i), None, None])),
                    ),
                    (
                        "note",
                        strings(ids.iter().flat_map(|_| [Some("中文😀".into()), None, None])),
                    ),
                ],
                Some((0..n).flat_map(|_| [true, false, true]).collect()),
            );
            let details = structure(
                vec![
                    (
                        "name",
                        strings(ids.iter().map(|i| Some(format!("record {i}")))),
                    ),
                    ("records", list(records, vec![3; n], None)),
                ],
                Some(ids.iter().map(|i| i % 37 == 0).collect()),
            );
            batch(vec![
                ("id", Arc::new(Int64Array::from(ids.clone()))),
                (
                    "amount",
                    decimal(
                        ids.iter()
                            .map(|&i| (i % 20 == 0).then_some(i as i128 * 1_000_000)),
                        20,
                        6,
                    )?,
                ),
                (
                    "event_date",
                    Arc::new(Date32Array::from(
                        ids.iter()
                            .map(|&i| (i % 23 == 0).then_some(day("2025-01-01") + (i % 365) as i32))
                            .collect::<Vec<_>>(),
                    )),
                ),
                (
                    "enabled",
                    Arc::new(BooleanArray::from(
                        ids.iter()
                            .map(|&i| (i % 29 == 0).then_some(i % 2 == 0))
                            .collect::<Vec<_>>(),
                    )),
                ),
                (
                    "items",
                    list_ints(
                        ids.iter()
                            .map(|&i| (i % 31 == 0).then(|| vec![Some(i), None, Some(i + 1)])),
                    ),
                ),
                ("details", details),
                (
                    "optional_text",
                    strings(
                        ids.iter()
                            .map(|i| (i % 41 == 0).then(|| format!("present {i}"))),
                    ),
                ),
                (
                    "literal",
                    strings(ids.iter().map(|i| {
                        Some(
                            match i % 5 {
                                0 => "",
                                1 => "NULL",
                                2 => "NaN",
                                3 => "null",
                                _ => "ordinary text",
                            }
                            .into(),
                        )
                    })),
                ),
            ])
        },
    )
}

pub fn make_wide(path: &Path) -> Result<()> {
    write_fixture(
        path,
        5000,
        512,
        Compression::ZSTD(Default::default()),
        |start, n| {
            let ids = (start..start + n).map(|i| i as i64).collect::<Vec<_>>();
            let mut arrays = Vec::with_capacity(1000);
            for col in 0..1000 {
                let present = |i: i64| (i + col as i64) % 23 != 0;
                let a: ArrayRef = match col {
                    0 => Arc::new(Int64Array::from(ids.clone())),
                    500 | 999 => strings(ids.iter().map(|i| {
                        Some(format!(
                            "{} · row={i} · col_{col:04}",
                            if col == 500 { "MIDDLE" } else { "LAST" }
                        ))
                    })),
                    250 => decimal(
                        ids.iter()
                            .map(|&i| present(i).then_some(LARGE_INTEGER_BASE + i as i128)),
                        38,
                        0,
                    )?,
                    c if c % 200 == 49 => list_ints(ids.iter().map(|&i| {
                        if !present(i) {
                            None
                        } else if i % 11 == 0 {
                            Some(vec![])
                        } else {
                            Some(vec![Some(i), Some(c as i64), None, Some(i + 1)])
                        }
                    })),
                    c => match c % 7 {
                        0 => ints(
                            ids.iter()
                                .map(|&i| present(i).then_some(i * 10000 + c as i64)),
                        ),
                        1 => decimal(
                            ids.iter().map(|&i| {
                                present(i).then_some(
                                    (9_007_199_254_740_993 + i as i128) * 1_000_000_000_000_000_000
                                        + 123_456_789_012_340_000
                                        + c as i128,
                                )
                            }),
                            38,
                            18,
                        )?,
                        2 => Arc::new(Date32Array::from(
                            ids.iter()
                                .map(|&i| {
                                    present(i).then_some(
                                        day("2025-01-01") + ((i + c as i64) % 365) as i32,
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )),
                        3 => Arc::new(BooleanArray::from(
                            ids.iter()
                                .map(|&i| present(i).then_some((i + c as i64) % 2 == 0))
                                .collect::<Vec<_>>(),
                        )),
                        4 => strings(ids.iter().map(|&i| {
                            present(i).then(|| match i % 5 {
                                0 => "".into(),
                                1 => "NULL".into(),
                                2 => "NaN".into(),
                                _ => format!("上海😀 · row={i} · col_{c:04}"),
                            })
                        })),
                        5 => Arc::new(Float64Array::from(
                            ids.iter()
                                .map(|&i| {
                                    present(i).then_some(if i % 101 == 0 {
                                        f64::NAN
                                    } else {
                                        (i + c as i64) as f64 / 10.
                                    })
                                })
                                .collect::<Vec<_>>(),
                        )),
                        _ => Arc::new(TimestampMicrosecondArray::from(
                            ids.iter()
                                .map(|&i| {
                                    present(i).then_some(
                                        micros("2025-01-01 12:34:56") + i * 1_000_000 + c as i64,
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )),
                    },
                };
                arrays.push((format!("col_{col:04}"), a));
            }
            Ok(RecordBatch::try_from_iter(arrays)?)
        },
    )
}

pub fn make_large(path: &Path) -> Result<()> {
    write_fixture(
        path,
        2_600_000,
        8192,
        Compression::UNCOMPRESSED,
        |start, n| {
            let ids = (start..start + n).map(|i| i as i64).collect::<Vec<_>>();
            let customer = structure(
                vec![
                    ("id", ints(ids.iter().map(|i| Some(i % 100003)))),
                    (
                        "city",
                        strings(
                            ids.iter()
                                .map(|i| Some(if i % 2 == 0 { "上海" } else { "杭州" }.into())),
                        ),
                    ),
                ],
                None,
            );
            let measurements =
                structure(
                    vec![
                        (
                            "kind",
                            strings(ids.iter().flat_map(|_| {
                                [Some("temperature".into()), Some("humidity".into())]
                            })),
                        ),
                        (
                            "reading",
                            Arc::new(Float64Array::from(
                                ids.iter()
                                    .flat_map(|&i| [i as f64 * 0.125, i as f64 * 0.03125])
                                    .collect::<Vec<_>>(),
                            )),
                        ),
                    ],
                    None,
                );
            let details = structure(
                vec![
                    ("customer", customer),
                    ("measurements", list(measurements, vec![2; n], None)),
                    (
                        "optional",
                        ints(ids.iter().map(|&i| (i % 7 != 0).then_some(i))),
                    ),
                    (
                        "payload",
                        strings(ids.iter().map(|&i| {
                            Some(
                                format!(
                                    "{i:032x}{:032x}",
                                    (i as u64).wrapping_mul(0x9e3779b97f4a7c15)
                                )
                                .repeat(32),
                            )
                        })),
                    ),
                ],
                None,
            );
            batch(vec![
                ("id", Arc::new(Int64Array::from(ids.clone()))),
                ("details", details),
                (
                    "note",
                    strings(
                        ids.iter()
                            .map(|i| (i % 11 != 0).then(|| format!("record {i}"))),
                    ),
                ),
            ])
        },
    )
}

pub fn make_demo(path: &Path) -> Result<()> {
    let image = preview_png();
    let samples = 8000u32;
    let mut wav = Vec::from(&b"RIFF"[..]);
    wav.extend_from_slice(&(36 + samples * 2).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&8000u32.to_le_bytes());
    wav.extend_from_slice(&16000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples * 2).to_le_bytes());
    wav.resize(wav.len() + samples as usize * 2, 0);
    write_fixture(
        path,
        5000,
        8192,
        Compression::ZSTD(Default::default()),
        |start, n| {
            let ids = (start..start + n).map(|i| i as i64).collect::<Vec<_>>();
            let children = structure(vec![("value", ints(ids.iter().copied().map(Some)))], None);
            let details = structure(
                vec![
                    (
                        "name",
                        strings(ids.iter().map(|i| Some(format!("nested {i}")))),
                    ),
                    ("children", list(children, vec![1; n], None)),
                ],
                None,
            );
            batch(vec![
                (
                    "id",
                    Arc::new(Int32Array::from(
                        ids.iter().map(|&i| i as i32).collect::<Vec<_>>(),
                    )),
                ),
                (
                    "VendorID",
                    Arc::new(Int32Array::from(
                        ids.iter().map(|&i| (i % 2 + 1) as i32).collect::<Vec<_>>(),
                    )),
                ),
                (
                    "fare_amount",
                    decimal(
                        ids.iter().map(|&i| Some((i % 90 + 10) as i128 * 125)),
                        12,
                        2,
                    )?,
                ),
                (
                    "tip_amount",
                    decimal(ids.iter().map(|&i| Some((i % 12) as i128 * 75)), 12, 2)?,
                ),
                (
                    "pickup_time",
                    Arc::new(TimestampMicrosecondArray::from(
                        ids.iter()
                            .map(|&i| micros("2024-01-01 09:00:00") + i * 60_000_000)
                            .collect::<Vec<_>>(),
                    )),
                ),
                (
                    "notes",
                    strings(
                        ids.iter()
                            .map(|i| (i % 7 != 0).then(|| format!("上海 — record {i}"))),
                    ),
                ),
                (
                    "active",
                    Arc::new(BooleanArray::from(
                        ids.iter().map(|i| i % 2 == 0).collect::<Vec<_>>(),
                    )),
                ),
                (
                    "values",
                    list_ints(ids.iter().map(|&i| Some(vec![Some(i), Some(i + 1)]))),
                ),
                ("details", details),
                (
                    "photo",
                    Arc::new(BinaryArray::from_iter(
                        ids.iter().map(|&i| (i == 0).then_some(image.as_slice())),
                    )),
                ),
                (
                    "sound",
                    Arc::new(BinaryArray::from_iter(
                        ids.iter().map(|&i| (i == 0).then_some(wav.as_slice())),
                    )),
                ),
            ])
        },
    )
}

fn preview_png() -> Vec<u8> {
    fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        output.extend_from_slice(&(data.len() as u32).to_be_bytes());
        output.extend_from_slice(kind);
        output.extend_from_slice(data);
        let mut crc = !0_u32;
        for byte in kind.iter().chain(data) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
            }
        }
        output.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut pixels = Vec::new();
    for y in 0..64 {
        pixels.push(0);
        for x in 0..64 {
            pixels.extend_from_slice(if (x / 8 + y / 8) % 2 == 0 {
                &[48, 112, 200, 255]
            } else {
                &[125, 88, 190, 255]
            });
        }
    }
    let mut zlib = vec![0x78, 0x01, 0x01];
    let len = pixels.len() as u16;
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&pixels);
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in pixels {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::from(64_u32.to_be_bytes());
    header.extend_from_slice(&64_u32.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}
