//! Small, typed test fixtures built with Arrow and the Parquet writer. No
//! database, source-file mutation or production row-processing engine is used.
use std::{fs::File, path::Path, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use arrow::{
    array::*,
    buffer::{NullBuffer, OffsetBuffer},
    datatypes::{DataType, SchemaRef},
    record_batch::RecordBatch,
};
use parquet::arrow::ArrowWriter;
use serde_json::Value;

use crate::{
    columnar::ArrowTable,
    engine::{Cell, ColumnInfo},
};

pub(crate) fn write_parquet(path: &Path, batches: &[RecordBatch]) -> Result<()> {
    let schema = batches
        .first()
        .context("A fixture needs a typed batch")?
        .schema();
    let mut writer = ArrowWriter::try_new(File::create_new(path)?, schema, None)?;
    for batch in batches {
        writer.write(batch)?;
    }
    writer.close()?;
    Ok(())
}

pub(crate) fn write_rows(path: &Path, columns: &[ColumnInfo], rows: &[Value]) -> Result<()> {
    let batch = batch_from_json(crate::columnar::fixture_schema(columns)?, rows)?;
    write_parquet(path, &[batch])
}

pub(crate) fn batch_from_json(schema: SchemaRef, rows: &[Value]) -> Result<RecordBatch> {
    let columns = schema
        .fields()
        .iter()
        .map(|field| {
            let values = rows
                .iter()
                .map(|row| row.get(field.name()).cloned().unwrap_or(Value::Null))
                .collect::<Vec<_>>();
            array_from_json(field.data_type(), &values)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordBatch::try_new(schema, columns)?)
}

pub(crate) fn table_from_cells(
    columns: Vec<ColumnInfo>,
    rows: Vec<Vec<Cell>>,
) -> Result<ArrowTable> {
    ensure!(
        rows.iter().all(|row| row.len() == columns.len()),
        "A fixture row differs from its schema"
    );
    let schema = crate::columnar::fixture_schema(&columns)?;
    let arrays = columns
        .iter()
        .enumerate()
        .map(|(column, info)| {
            let values = rows
                .iter()
                .map(|row| {
                    let cell = &row[column];
                    if cell.is_null {
                        Value::Null
                    } else if info.data_type == "BLOB" {
                        serde_json::json!(cell.bytes.clone().unwrap_or_default())
                    } else {
                        cell.json
                            .clone()
                            .unwrap_or_else(|| Value::String(cell.text.clone()))
                    }
                })
                .collect::<Vec<_>>();
            array_from_json(schema.field(column).data_type(), &values)
        })
        .collect::<Result<Vec<_>>>()?;
    let batch = RecordBatch::try_new(schema.clone(), arrays)?;
    ArrowTable::new_with_schema(columns, vec![batch], schema)
}

/// Recursion follows schema fields, while Arrow performs scalar conversion and
/// owns every resulting typed buffer. This adapter is only fixture authoring.
pub(crate) fn array_from_json(kind: &DataType, values: &[Value]) -> Result<ArrayRef> {
    let nulls = || {
        let validity = values
            .iter()
            .map(|value| !value.is_null())
            .collect::<Vec<_>>();
        validity
            .iter()
            .any(|valid| !valid)
            .then(|| NullBuffer::from(validity))
    };
    match kind {
        DataType::Null => Ok(Arc::new(NullArray::new(values.len()))),
        DataType::Struct(fields) => {
            let arrays = fields
                .iter()
                .map(|field| {
                    let children = values
                        .iter()
                        .map(|value| value.get(field.name()).cloned().unwrap_or(Value::Null))
                        .collect::<Vec<_>>();
                    array_from_json(field.data_type(), &children)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Arc::new(StructArray::try_new(
                fields.clone(),
                arrays,
                nulls(),
            )?))
        }
        DataType::List(field) | DataType::LargeList(field) | DataType::FixedSizeList(field, _) => {
            let mut lengths = Vec::with_capacity(values.len());
            let mut children = Vec::new();
            for value in values {
                if let Some(array) = value.as_array() {
                    if let DataType::FixedSizeList(_, size) = kind {
                        ensure!(
                            array.len() == *size as usize,
                            "Fixed-size fixture list length differs"
                        );
                    }
                    lengths.push(array.len());
                    children.extend(array.iter().cloned());
                } else if value.is_null() {
                    let count = if let DataType::FixedSizeList(_, size) = kind {
                        *size as usize
                    } else {
                        0
                    };
                    lengths.push(count);
                    children.resize(children.len() + count, Value::Null);
                } else {
                    bail!("A List fixture needs an array");
                }
            }
            let children = array_from_json(field.data_type(), &children)?;
            Ok(match kind {
                DataType::List(_) => Arc::new(ListArray::try_new(
                    field.clone(),
                    OffsetBuffer::<i32>::from_lengths(lengths),
                    children,
                    nulls(),
                )?) as ArrayRef,
                DataType::LargeList(_) => Arc::new(LargeListArray::try_new(
                    field.clone(),
                    OffsetBuffer::<i64>::from_lengths(lengths),
                    children,
                    nulls(),
                )?) as ArrayRef,
                DataType::FixedSizeList(_, size) => Arc::new(FixedSizeListArray::try_new(
                    field.clone(),
                    *size,
                    children,
                    nulls(),
                )?) as ArrayRef,
                _ => unreachable!(),
            })
        }
        DataType::Map(field, sorted) => {
            let DataType::Struct(fields) = field.data_type() else {
                bail!("A Map fixture needs key/value fields");
            };
            let mut lengths = Vec::with_capacity(values.len());
            let (mut keys, mut entries) = (Vec::new(), Vec::new());
            for value in values {
                if let Some(object) = value.as_object() {
                    lengths.push(object.len());
                    for (key, value) in object {
                        keys.push(Value::String(key.clone()));
                        entries.push(value.clone());
                    }
                } else if let Some(pairs) = value.as_array() {
                    lengths.push(pairs.len());
                    for pair in pairs {
                        keys.push(pair["key"].clone());
                        entries.push(pair["value"].clone());
                    }
                } else if value.is_null() {
                    lengths.push(0);
                } else {
                    bail!("A Map fixture needs an object or key/value array");
                }
            }
            let entries = StructArray::try_new(
                fields.clone(),
                vec![
                    array_from_json(fields[0].data_type(), &keys)?,
                    array_from_json(fields[1].data_type(), &entries)?,
                ],
                None,
            )?;
            Ok(Arc::new(MapArray::try_new(
                field.clone(),
                OffsetBuffer::<i32>::from_lengths(lengths),
                entries,
                nulls(),
                *sorted,
            )?))
        }
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => {
            let data = values
                .iter()
                .map(|value| -> Result<Option<Vec<u8>>> {
                    if value.is_null() {
                        return Ok(None);
                    }
                    let array = value
                        .as_array()
                        .context("A Binary fixture needs byte values")?;
                    Ok(Some(
                        array
                            .iter()
                            .map(|byte| {
                                u8::try_from(byte.as_u64().context("Invalid byte")?)
                                    .context("Byte exceeds 255")
                            })
                            .collect::<Result<Vec<_>>>()?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let binary = BinaryArray::from_iter(data.iter().map(Option::as_deref));
            Ok(arrow::compute::cast(&binary, kind)?)
        }
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => {
            let text = values
                .iter()
                .map(|value| {
                    (!value.is_null()).then(|| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    })
                })
                .collect::<Vec<_>>();
            Ok(arrow::compute::cast(&StringArray::from(text), kind)?)
        }
        _ => {
            let text = values
                .iter()
                .map(|value| {
                    (!value.is_null()).then(|| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    })
                })
                .collect::<Vec<_>>();
            Ok(arrow::compute::cast(&StringArray::from(text), kind)?)
        }
    }
}
