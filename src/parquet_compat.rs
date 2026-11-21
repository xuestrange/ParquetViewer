//! Lossless bridge for Parquet's legacy 12-byte INTERVAL representation.
//!
//! Arrow 58 selects either YearMonth or DayTime and drops the other components.
//! Preserve the physical bytes instead. Only logical schema metadata changes;
//! the Parquet decoder still performs projection, row selection and decoding.
use anyhow::{Result, ensure};
use arrow::{
    array::{Array, ArrayRef, Decimal128Array, FixedSizeBinaryArray, make_array},
    datatypes::{DataType, Field, FieldRef, Fields, Schema},
};
use parquet::{
    arrow::{
        ArrowSchemaConverter,
        arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions},
        arrow_writer::ArrowWriterOptions,
    },
    basic::{ConvertedType, Type as PhysicalType},
    file::{properties::WriterProperties, reader::ChunkReader},
    schema::types::{SchemaDescriptor, Type},
};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};

const LOGICAL_KEY: &str = "parquetviewer.logical_type";
const INTERVAL_VALUE: &str = "INTERVAL_MONTH_DAY_MILLISECOND_LE";
const ORIGINAL_STORAGE_KEY: &str = "parquetviewer.interval.original_arrow_type";
const MAX_DEPTH: usize = 256;

fn raw_interval_type(kind: &DataType) -> bool {
    match kind {
        DataType::FixedSizeBinary(12) => true,
        DataType::Dictionary(_, value) => raw_interval_type(value),
        _ => false,
    }
}
pub fn is_interval_field(field: &Field) -> bool {
    field
        .metadata()
        .get(LOGICAL_KEY)
        .is_some_and(|value| value == INTERVAL_VALUE)
        && raw_interval_type(field.data_type())
}
pub fn field_label(field: &Field) -> Option<&'static str> {
    is_interval_field(field).then_some("INTERVAL")
}
fn marked_field(mut field: Field, original: &DataType) -> Field {
    let mut metadata = field.metadata().clone();
    metadata.insert(LOGICAL_KEY.into(), INTERVAL_VALUE.into());
    metadata
        .entry(ORIGINAL_STORAGE_KEY.into())
        .or_insert_with(|| format!("{original:?}"));
    field = field.with_metadata(metadata);
    field
}
fn fields_of(kind: &DataType) -> Vec<FieldRef> {
    match kind {
        DataType::Struct(fields) => fields.iter().cloned().collect(),
        DataType::List(field)
        | DataType::LargeList(field)
        | DataType::ListView(field)
        | DataType::LargeListView(field)
        | DataType::FixedSizeList(field, _)
        | DataType::Map(field, _) => vec![field.clone()],
        DataType::Dictionary(_, value) => vec![Arc::new(Field::new(
            "dictionary_value",
            value.as_ref().clone(),
            true,
        ))],
        DataType::RunEndEncoded(runs, values) => vec![runs.clone(), values.clone()],
        DataType::Union(fields, _) => fields.iter().map(|(_, field)| field.clone()).collect(),
        _ => Vec::new(),
    }
}
fn container_type(kind: &DataType) -> bool {
    matches!(
        kind,
        DataType::Struct(_)
            | DataType::List(_)
            | DataType::LargeList(_)
            | DataType::ListView(_)
            | DataType::LargeListView(_)
            | DataType::FixedSizeList(_, _)
            | DataType::Map(_, _)
            | DataType::Dictionary(_, _)
            | DataType::RunEndEncoded(_, _)
            | DataType::Union(_, _)
    )
}
fn map_children(
    kind: &DataType,
    mut map: impl FnMut(&FieldRef) -> Result<FieldRef>,
) -> Result<DataType> {
    Ok(match kind {
        DataType::Struct(fields) => {
            DataType::Struct(fields.iter().map(&mut map).collect::<Result<Fields>>()?)
        }
        DataType::List(field) => DataType::List(map(field)?),
        DataType::LargeList(field) => DataType::LargeList(map(field)?),
        DataType::ListView(field) => DataType::ListView(map(field)?),
        DataType::LargeListView(field) => DataType::LargeListView(map(field)?),
        DataType::FixedSizeList(field, count) => DataType::FixedSizeList(map(field)?, *count),
        DataType::Map(field, sorted) => DataType::Map(map(field)?, *sorted),
        DataType::Dictionary(keys, value) => {
            let field = map(&Arc::new(Field::new(
                "dictionary_value",
                value.as_ref().clone(),
                true,
            )))?;
            DataType::Dictionary(keys.clone(), Box::new(field.data_type().clone()))
        }
        DataType::RunEndEncoded(runs, values) => DataType::RunEndEncoded(map(runs)?, map(values)?),
        DataType::Union(fields, mode) => DataType::Union(
            fields
                .iter()
                .map(|(id, field)| Ok((id, map(field)?)))
                .collect::<Result<_>>()?,
            *mode,
        ),
        _ => kind.clone(),
    })
}
fn rewrite_parquet(
    schema: &SchemaDescriptor,
    leaves: &HashSet<usize>,
    converted: ConvertedType,
) -> Result<SchemaDescriptor> {
    fn visit(
        kind: &Type,
        leaves: &HashSet<usize>,
        converted: ConvertedType,
        index: &mut usize,
        depth: usize,
    ) -> Result<Type> {
        ensure!(
            depth <= MAX_DEPTH,
            "Parquet INTERVAL schema exceeds supported nesting depth"
        );
        match kind {
            Type::GroupType { basic_info, fields } => Ok(Type::GroupType {
                basic_info: basic_info.clone(),
                fields: fields
                    .iter()
                    .map(|field| Ok(Arc::new(visit(field, leaves, converted, index, depth + 1)?)))
                    .collect::<Result<Vec<_>>>()?,
            }),
            Type::PrimitiveType {
                basic_info,
                physical_type,
                type_length,
                scale,
                precision,
            } => {
                let selected = leaves.contains(index);
                *index += 1;
                if !selected {
                    return Ok(kind.clone());
                }
                ensure!(
                    *physical_type == PhysicalType::FIXED_LEN_BYTE_ARRAY && *type_length == 12,
                    "Parquet INTERVAL must contain exactly 12 physical bytes"
                );
                Ok(
                    Type::primitive_type_builder(basic_info.name(), *physical_type)
                        .with_repetition(basic_info.repetition())
                        .with_length(*type_length)
                        .with_scale(*scale)
                        .with_precision(*precision)
                        .with_id(basic_info.has_id().then(|| basic_info.id()))
                        .with_converted_type(converted)
                        .with_logical_type(None)
                        .build()?,
                )
            }
        }
    }
    Ok(SchemaDescriptor::new(Arc::new(visit(
        schema.root_schema(),
        leaves,
        converted,
        &mut 0,
        0,
    )?)))
}
fn raw_schema(schema: &Schema, leaves: &HashSet<usize>) -> Result<Schema> {
    fn rewrite_field(
        field: &FieldRef,
        leaves: &HashSet<usize>,
        index: &mut usize,
        depth: usize,
    ) -> Result<FieldRef> {
        ensure!(
            depth <= MAX_DEPTH,
            "Arrow INTERVAL schema exceeds supported nesting depth"
        );
        let original = field.data_type();
        // A dictionary's value is the same physical leaf. Keep its encoding,
        // but put the logical marker on the real field rather than a synthetic
        // dictionary value Field which Arrow cannot retain.
        fn storage(
            kind: &DataType,
            leaves: &HashSet<usize>,
            index: &mut usize,
            depth: usize,
        ) -> Result<(DataType, bool)> {
            if let DataType::Dictionary(keys, values) = kind {
                let (value, marked) = storage(values, leaves, index, depth + 1)?;
                return Ok((DataType::Dictionary(keys.clone(), Box::new(value)), marked));
            }
            if !container_type(kind) {
                let marked = leaves.contains(index);
                *index += 1;
                return Ok((
                    if marked {
                        DataType::FixedSizeBinary(12)
                    } else {
                        kind.clone()
                    },
                    marked,
                ));
            }
            Ok((
                map_children(kind, |child| rewrite_field(child, leaves, index, depth + 1))?,
                false,
            ))
        }
        let (kind, marked) = storage(original, leaves, index, depth)?;
        let output = field.as_ref().clone().with_data_type(kind);
        Ok(Arc::new(if marked {
            marked_field(output, original)
        } else {
            output
        }))
    }
    let mut index = 0;
    let fields = schema
        .fields()
        .iter()
        .map(|f| rewrite_field(f, leaves, &mut index, 0))
        .collect::<Result<Fields>>()?;
    ensure!(
        leaves.iter().all(|leaf| *leaf < index),
        "Parquet INTERVAL leaf mapping differs from Arrow schema"
    );
    Ok(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

/// Load metadata for the ordinary Arrow reader, selecting raw FixedSizeBinary
/// storage for every physical legacy INTERVAL leaf, even with an Arrow unit
/// hint. A unit hint alone cannot prove that the other physical components are
/// zero. All unrelated fields, metadata and physical columns remain unchanged.
pub fn load_metadata<T: ChunkReader>(reader: &T) -> Result<ArrowReaderMetadata> {
    let original = ArrowReaderMetadata::load(reader, ArrowReaderOptions::new())?;
    let physical = original.parquet_schema();
    let leaves = physical
        .columns()
        .iter()
        .enumerate()
        .filter_map(|(index, column)| {
            (column.converted_type() == ConvertedType::INTERVAL).then_some(index)
        })
        .collect::<HashSet<_>>();
    if leaves.is_empty() {
        return Ok(original);
    }
    let schema = Arc::new(raw_schema(original.schema(), &leaves)?);
    let physical = Arc::new(rewrite_parquet(physical, &leaves, ConvertedType::NONE)?);
    // with_schema alone is intentionally insufficient: Arrow's normal
    // INTERVAL inference rejects a FixedSizeBinary hint. Neutralizing only the
    // logical annotation lets the unchanged physical decoder return all bytes.
    Ok(ArrowReaderMetadata::load(
        reader,
        ArrowReaderOptions::new()
            .with_parquet_schema(physical)
            .with_schema(schema),
    )?)
}

/// Preserve all 12 bytes and restore the original Parquet INTERVAL annotation
/// when exporting a bridged snapshot. Standard Arrow metadata records the raw
/// storage plus logical marker so this application can reopen it losslessly.
pub fn writer_options(schema: &Schema, properties: WriterProperties) -> Result<ArrowWriterOptions> {
    fn collect(
        field: &Field,
        index: &mut usize,
        leaves: &mut HashSet<usize>,
        depth: usize,
    ) -> Result<()> {
        ensure!(
            depth <= MAX_DEPTH,
            "Arrow INTERVAL export schema exceeds supported nesting depth"
        );
        if is_interval_field(field) {
            leaves.insert(*index);
            *index += 1;
            return Ok(());
        }
        let children = fields_of(field.data_type());
        if !container_type(field.data_type()) {
            *index += 1;
        } else {
            for child in children {
                collect(&child, index, leaves, depth + 1)?;
            }
        }
        Ok(())
    }
    let mut leaves = HashSet::new();
    let mut index = 0;
    for field in schema.fields() {
        collect(field, &mut index, &mut leaves, 0)?;
    }
    let options = ArrowWriterOptions::new().with_properties(properties.clone());
    if leaves.is_empty() {
        return Ok(options);
    }
    let physical = ArrowSchemaConverter::new()
        .with_coerce_types(properties.coerce_types())
        .convert(schema)?;
    Ok(options.with_parquet_schema(rewrite_parquet(
        &physical,
        &leaves,
        ConvertedType::INTERVAL,
    )?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalParts {
    // Match the previous reader's interpretation of the three physical words:
    // signed months/days, unsigned milliseconds. Every original bit survives.
    pub months: i32,
    pub days: i32,
    pub milliseconds: u32,
}
impl IntervalParts {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (bytes.len() == 12).then(|| Self {
            months: i32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            days: i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            milliseconds: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        })
    }
    /// Preserve the previous viewer's comparison semantics: a month is 30 days
    /// and a day is 24 hours for equality/ordering only. No source values change.
    /// https://duckdb.org/docs/1.5/sql/data_types/interval#equality-and-comparison
    pub fn comparison_milliseconds(self) -> i128 {
        self.months as i128 * 30 * 86_400_000
            + self.days as i128 * 86_400_000
            + self.milliseconds as i128
    }

    /// Same scalar spelling as the previous viewer, without a query runtime.
    /// Preserve the independent day/time bases; 48 hours stays `48:00:00`.
    pub fn canonical_text(self) -> String {
        let mut pieces = Vec::with_capacity(4);
        for (value, unit) in [
            (self.months / 12, "year"),
            (self.months % 12, "month"),
            (self.days, "day"),
        ] {
            if value != 0 {
                pieces.push(format!(
                    "{value} {unit}{}",
                    if matches!(value, -1 | 1) { "" } else { "s" }
                ));
            }
        }
        if self.milliseconds != 0 {
            let hours = self.milliseconds / 3_600_000;
            let minutes = self.milliseconds / 60_000 % 60;
            let seconds = self.milliseconds / 1_000 % 60;
            let fraction = self.milliseconds % 1_000;
            let mut time = format!("{hours:02}:{minutes:02}:{seconds:02}");
            if fraction != 0 {
                time.push('.');
                time.push_str(format!("{fraction:03}").trim_end_matches('0'));
            }
            pieces.push(time);
        }
        if pieces.is_empty() {
            "00:00:00".into()
        } else {
            pieces.join(" ")
        }
    }
}
pub fn interval_parts(array: &dyn Array, row: usize) -> Option<IntervalParts> {
    if row >= array.len() || array.is_null(row) {
        return None;
    }
    let array = array.as_any().downcast_ref::<FixedSizeBinaryArray>()?;
    IntervalParts::from_bytes(array.value(row))
}
pub fn interval_text(array: &dyn Array, row: usize) -> Option<String> {
    Some(interval_parts(array, row)?.canonical_text())
}
pub fn interval_json(array: &dyn Array, row: usize) -> Option<Value> {
    interval_text(array, row).map(Value::String)
}
pub fn contains_interval(field: &Field) -> bool {
    is_interval_field(field)
        || fields_of(field.data_type())
            .iter()
            .any(|child| contains_interval(child))
}

/// A field-aware adapter to apply before the existing Map/List sort adapter.
/// Ordinary fields retain their exact DataType and original buffers.
pub fn interval_sort_type(field: &Field) -> DataType {
    if is_interval_field(field) {
        return DataType::Decimal128(38, 0);
    }
    map_children(field.data_type(), |child| {
        Ok(Arc::new(
            child
                .as_ref()
                .clone()
                .with_data_type(interval_sort_type(child)),
        ))
    })
    .expect("infallible interval sort type mapping")
}
pub fn interval_sort_array(field: &Field, array: &ArrayRef) -> Result<ArrayRef> {
    if !contains_interval(field) {
        return Ok(array.clone());
    }
    if is_interval_field(field) {
        let decoded = arrow::compute::cast(array, &DataType::FixedSizeBinary(12))?;
        let values = (0..decoded.len()).map(|row| {
            interval_parts(decoded.as_ref(), row).map(IntervalParts::comparison_milliseconds)
        });
        return Ok(Arc::new(
            Decimal128Array::from_iter(values).with_precision_and_scale(38, 0)?,
        ));
    }
    let data = array.to_data();
    let fields = fields_of(field.data_type());
    ensure!(
        fields.len() == data.child_data().len(),
        "INTERVAL sort container layout differs from its Arrow schema"
    );
    let children = fields
        .iter()
        .zip(data.child_data())
        .map(|(field, child)| Ok(interval_sort_array(field, &make_array(child.clone()))?.to_data()))
        .collect::<Result<Vec<_>>>()?;
    Ok(make_array(
        data.into_builder()
            .data_type(interval_sort_type(field))
            .child_data(children)
            .build()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use arrow::{
        array::{FixedSizeBinaryBuilder, Int64Array, ListArray, StringArray, StructArray},
        buffer::{NullBuffer, OffsetBuffer, ScalarBuffer},
        datatypes::IntervalUnit,
        record_batch::RecordBatch,
    };
    use parquet::{
        arrow::{
            ArrowWriter, add_encoded_arrow_schema_to_metadata,
            arrow_reader::ParquetRecordBatchReaderBuilder,
        },
        data_type::{FixedLenByteArray, FixedLenByteArrayType},
        file::writer::SerializedFileWriter,
        schema::parser::parse_message_type,
    };
    use std::{fs::File, path::Path};

    fn encoded(parts: IntervalParts) -> [u8; 12] {
        let mut value = [0; 12];
        value[..4].copy_from_slice(&parts.months.to_le_bytes());
        value[4..8].copy_from_slice(&parts.days.to_le_bytes());
        value[8..].copy_from_slice(&parts.milliseconds.to_le_bytes());
        value
    }
    fn array(values: &[Option<IntervalParts>]) -> ArrayRef {
        let mut array = FixedSizeBinaryBuilder::with_capacity(values.len(), 12);
        for value in values {
            if let Some(value) = value {
                array.append_value(encoded(*value)).unwrap();
            } else {
                array.append_null();
            }
        }
        Arc::new(array.finish())
    }
    fn interval_field(name: &str, nullable: bool) -> Field {
        marked_field(
            Field::new(name, DataType::FixedSizeBinary(12), nullable),
            &DataType::Interval(IntervalUnit::MonthDayNano),
        )
    }
    fn read(path: &Path) -> Result<RecordBatch> {
        let file = File::open(path)?;
        let metadata = load_metadata(&file)?;
        Ok(
            ParquetRecordBatchReaderBuilder::new_with_metadata(file, metadata)
                .with_batch_size(8192)
                .build()?
                .next()
                .context("Missing interval batch")??,
        )
    }
    fn write(path: &Path, batch: &RecordBatch, skip_metadata: bool) -> Result<()> {
        let options = writer_options(&batch.schema(), WriterProperties::builder().build())?
            .with_skip_arrow_metadata(skip_metadata);
        let mut writer =
            ArrowWriter::try_new_with_options(File::create_new(path)?, batch.schema(), options)?;
        writer.write(batch)?;
        writer.close()?;
        Ok(())
    }
    fn physical_fixture(
        path: &Path,
        values: &[Option<IntervalParts>],
        hint: Option<IntervalUnit>,
    ) -> Result<()> {
        let schema = Arc::new(parse_message_type(
            "message schema { OPTIONAL FIXED_LEN_BYTE_ARRAY(12) value (INTERVAL); }",
        )?);
        let mut props = WriterProperties::builder().build();
        if let Some(unit) = hint {
            add_encoded_arrow_schema_to_metadata(
                &Schema::new(vec![Field::new("value", DataType::Interval(unit), true)]),
                &mut props,
            );
        }
        let mut writer =
            SerializedFileWriter::new(File::create_new(path)?, schema, Arc::new(props))?;
        let mut group = writer.next_row_group()?;
        let mut column = group.next_column()?.unwrap();
        let bytes = values
            .iter()
            .flatten()
            .map(|parts| FixedLenByteArray::from(encoded(*parts).to_vec()))
            .collect::<Vec<_>>();
        let levels = values
            .iter()
            .map(|value| i16::from(value.is_some()))
            .collect::<Vec<_>>();
        column
            .typed::<FixedLenByteArrayType>()
            .write_batch(&bytes, Some(&levels), None)?;
        column.close()?;
        group.close()?;
        writer.close()?;
        Ok(())
    }

    #[test]
    fn physical_legacy_interval_keeps_all_components_with_or_without_arrow_unit_hints() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let expected = [
            Some(IntervalParts {
                months: 1,
                days: 2,
                milliseconds: 3,
            }),
            None,
            Some(IntervalParts {
                months: -1,
                days: -2,
                milliseconds: u32::MAX - 2,
            }),
            Some(IntervalParts {
                months: 0,
                days: 40,
                milliseconds: 256,
            }),
        ];
        for (index, hint) in [
            None,
            Some(IntervalUnit::DayTime),
            Some(IntervalUnit::YearMonth),
            Some(IntervalUnit::MonthDayNano),
        ]
        .into_iter()
        .enumerate()
        {
            let path = directory.path().join(format!("legacy-{index}.parquet"));
            physical_fixture(&path, &expected, hint)?;
            let native = ParquetRecordBatchReaderBuilder::try_new(File::open(&path)?)?;
            assert_eq!(
                native.parquet_schema().column(0).converted_type(),
                ConvertedType::INTERVAL
            );
            if index == 0 {
                assert!(
                    native
                        .metadata()
                        .file_metadata()
                        .key_value_metadata()
                        .is_none_or(|items| items.iter().all(|item| item.key != "ARROW:schema"))
                );
            }
            let batch = read(&path)?;
            assert_eq!(field_label(batch.schema().field(0)), Some("INTERVAL"));
            assert_eq!(batch.column(0).data_type(), &DataType::FixedSizeBinary(12));
            for (row, value) in expected.iter().enumerate() {
                assert_eq!(interval_parts(batch.column(0).as_ref(), row), *value);
            }
            let exported = directory.path().join(format!("exported-{index}.parquet"));
            write(&exported, &batch, false)?;
            let physical = ParquetRecordBatchReaderBuilder::try_new(File::open(&exported)?)?;
            assert_eq!(
                physical.parquet_schema().column(0).converted_type(),
                ConvertedType::INTERVAL
            );
            let roundtrip = read(&exported)?;
            assert_eq!(roundtrip.schema(), batch.schema());
            assert_eq!(roundtrip.column(0).to_data(), batch.column(0).to_data());
            let twice = directory.path().join(format!("twice-{index}.parquet"));
            write(&twice, &roundtrip, false)?;
            assert_eq!(read(&twice)?.column(0).to_data(), batch.column(0).to_data());
        }
        Ok(())
    }

    #[test]
    fn nested_interval_nulls_lists_and_maps_roundtrip_without_changing_other_fields() -> Result<()>
    {
        use arrow::array::{MapArray, MapBuilder, StringBuilder};
        let directory = tempfile::tempdir()?;
        let a = IntervalParts {
            months: 7,
            days: 8,
            milliseconds: 9,
        };
        let b = IntervalParts {
            months: -4,
            days: 5,
            milliseconds: u32::MAX - 5,
        };
        let interval = Arc::new(interval_field("period", true));
        let nested = StructArray::new(
            vec![
                interval.clone(),
                Arc::new(Field::new("name", DataType::Utf8, false)),
            ]
            .into(),
            vec![
                array(&[Some(a), Some(b), None]),
                Arc::new(StringArray::from(vec!["first", "hidden", "last"])),
            ],
            Some(NullBuffer::from(vec![true, false, true])),
        );
        let list = ListArray::new(
            interval.clone(),
            OffsetBuffer::new(ScalarBuffer::from(vec![0i32, 2, 2, 3])),
            array(&[Some(a), None, Some(b)]),
            Some(NullBuffer::from(vec![true, false, true])),
        );
        let mut map = MapBuilder::new(None, StringBuilder::new(), FixedSizeBinaryBuilder::new(12));
        map.keys().append_value("one");
        map.values().append_value(encoded(a))?;
        map.append(true)?;
        map.append(false)?;
        map.keys().append_value("two");
        map.values().append_value(encoded(b))?;
        map.append(true)?;
        let map = map.finish();
        let entries = map.entries();
        let fields = vec![
            entries.fields()[0].clone(),
            Arc::new(interval_field(entries.fields()[1].name(), true)),
        ];
        let entries = StructArray::new(
            fields.into(),
            entries.columns().to_vec(),
            entries.nulls().cloned(),
        );
        let map = MapArray::new(
            Arc::new(Field::new("entries", entries.data_type().clone(), false)),
            map.offsets().clone(),
            entries,
            map.nulls().cloned(),
            false,
        );
        let batch = RecordBatch::try_from_iter([
            ("id", Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef),
            ("nested", Arc::new(nested)),
            ("list", Arc::new(list)),
            ("mapping", Arc::new(map)),
        ])?;
        let source = directory.path().join("nested.parquet");
        write(&source, &batch, false)?;
        let loaded = read(&source)?;
        assert_eq!(loaded.schema(), batch.schema());
        for index in 0..batch.num_columns() {
            assert_eq!(
                loaded.column(index).to_data(),
                batch.column(index).to_data()
            );
        }
        let exported = directory.path().join("nested-exported.parquet");
        write(&exported, &loaded, false)?;
        let again = read(&exported)?;
        assert_eq!(loaded.schema(), again.schema());
        for index in 0..batch.num_columns() {
            assert_eq!(
                loaded.column(index).to_data(),
                again.column(index).to_data()
            );
        }
        let projected = batch.project(&[3, 0, 2])?;
        let projected_path = directory.path().join("projected.parquet");
        write(&projected_path, &projected, false)?;
        let reopened_projection = read(&projected_path)?;
        assert_eq!(reopened_projection.schema(), projected.schema());
        for index in 0..projected.num_columns() {
            assert_eq!(
                reopened_projection.column(index).to_data(),
                projected.column(index).to_data()
            );
        }
        let field = loaded.schema().field(1).clone();
        let key = interval_sort_array(&field, loaded.column(1))?;
        let key = key.as_any().downcast_ref::<StructArray>().unwrap();
        assert_eq!(key.nulls(), loaded.column(1).nulls());
        assert_eq!(
            key.column(0)
                .as_any()
                .downcast_ref::<Decimal128Array>()
                .unwrap()
                .value(0),
            a.comparison_milliseconds()
        );
        Ok(())
    }

    #[test]
    fn normalized_interval_sort_keys_match_previous_comparison_semantics_and_keep_nulls()
    -> Result<()> {
        use arrow::compute::{SortOptions, sort_to_indices};
        let values = [
            Some(IntervalParts {
                months: 1,
                days: 0,
                milliseconds: 0,
            }),
            Some(IntervalParts {
                months: 0,
                days: 40,
                milliseconds: 0,
            }),
            Some(IntervalParts {
                months: -1,
                days: 0,
                milliseconds: 0,
            }),
            Some(IntervalParts {
                months: 0,
                days: 30,
                milliseconds: 0,
            }),
            Some(IntervalParts {
                months: 0,
                days: 0,
                milliseconds: 256,
            }),
            Some(IntervalParts {
                months: 0,
                days: 0,
                milliseconds: 1,
            }),
            None,
        ];
        let source = array(&values);
        let field = interval_field("value", true);
        let key = interval_sort_array(&field, &source)?;
        let key = key.as_any().downcast_ref::<Decimal128Array>().unwrap();
        assert_eq!(key.value(0), key.value(3));
        assert!(key.is_null(6));
        assert_eq!(source.data_type(), &DataType::FixedSizeBinary(12));
        let indices = sort_to_indices(
            key,
            Some(SortOptions {
                descending: false,
                nulls_first: false,
            }),
            None,
        )?;
        let ordered = indices
            .iter()
            .map(|index| index.unwrap() as usize)
            .collect::<Vec<_>>();
        assert_eq!(&ordered[..3], &[2, 5, 4]);
        assert_eq!(&ordered[5..], &[1, 6]);
        assert_eq!(
            interval_json(source.as_ref(), 0),
            Some(Value::String("1 month".into()))
        );
        assert_eq!(
            interval_text(source.as_ref(), 4),
            Some("00:00:00.256".into())
        );
        Ok(())
    }
    #[test]
    fn canonical_scalar_text_preserves_legacy_units_and_fraction_spelling() {
        for (parts, expected) in [
            (
                IntervalParts {
                    months: 0,
                    days: 0,
                    milliseconds: 0,
                },
                "00:00:00",
            ),
            (
                IntervalParts {
                    months: 13,
                    days: 2,
                    milliseconds: 3,
                },
                "1 year 1 month 2 days 00:00:00.003",
            ),
            (
                IntervalParts {
                    months: -13,
                    days: -1,
                    milliseconds: 100,
                },
                "-1 year -1 month -1 day 00:00:00.1",
            ),
            (
                IntervalParts {
                    months: 24,
                    days: 0,
                    milliseconds: 10,
                },
                "2 years 00:00:00.01",
            ),
            (
                IntervalParts {
                    months: 0,
                    days: 0,
                    milliseconds: u32::MAX,
                },
                "1193:02:47.295",
            ),
        ] {
            assert_eq!(parts.canonical_text(), expected);
        }
    }

    #[test]
    fn native_baseline_golden_text_json_and_bidirectional_stable_sort_match() -> Result<()> {
        use arrow::{
            array::UInt64Array,
            compute::{SortColumn, SortOptions, lexsort_to_indices},
        };
        // Actual baseline report: target/migration-interval-native-golden/report.json.
        // Keep the permanent test self-contained instead of relying on build artifacts.
        let parts = [
            None,
            Some(IntervalParts {
                months: 1,
                days: 2,
                milliseconds: 3,
            }),
            Some(IntervalParts {
                months: 0,
                days: 40,
                milliseconds: 0,
            }),
            Some(IntervalParts {
                months: 0,
                days: 30,
                milliseconds: 0,
            }),
            Some(IntervalParts {
                months: 0,
                days: 0,
                milliseconds: u32::MAX,
            }),
            Some(IntervalParts {
                months: 0,
                days: 0,
                milliseconds: 1,
            }),
            Some(IntervalParts {
                months: 0,
                days: 0,
                milliseconds: 256,
            }),
            Some(IntervalParts {
                months: 1,
                days: 0,
                milliseconds: 0,
            }),
        ];
        let expected = [
            None,
            Some("1 month 2 days 00:00:00.003"),
            Some("40 days"),
            Some("30 days"),
            Some("1193:02:47.295"),
            Some("00:00:00.001"),
            Some("00:00:00.256"),
            Some("1 month"),
        ];
        let source = array(&parts);
        for (row, text) in expected.into_iter().enumerate() {
            assert_eq!(interval_text(source.as_ref(), row).as_deref(), text);
            assert_eq!(
                interval_json(source.as_ref(), row),
                text.map(|text| Value::String(text.into()))
            );
        }
        let field = interval_field("value", true);
        let key = interval_sort_array(&field, &source)?;
        let ids = Arc::new(UInt64Array::from_iter_values(1..=8)) as ArrayRef;
        for (descending, expected) in [
            (false, [6, 7, 4, 8, 2, 3, 5, 1]),
            (true, [5, 3, 2, 4, 8, 7, 6, 1]),
        ] {
            let indices = lexsort_to_indices(
                &[
                    SortColumn {
                        values: key.clone(),
                        options: Some(SortOptions {
                            descending,
                            nulls_first: false,
                        }),
                    },
                    SortColumn {
                        values: ids.clone(),
                        options: Some(SortOptions {
                            descending: false,
                            nulls_first: false,
                        }),
                    },
                ],
                None,
            )?;
            assert_eq!(
                indices
                    .values()
                    .iter()
                    .map(|&row| row as usize + 1)
                    .collect::<Vec<_>>(),
                expected
            );
        }
        Ok(())
    }
    #[test]
    fn empty_struct_metadata_does_not_shift_following_interval_leaf_ordinals() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("empty", DataType::Struct(Fields::empty()), true),
            Field::new("value", DataType::Interval(IntervalUnit::DayTime), true),
        ]);
        let raw = raw_schema(&schema, &HashSet::from([0]))?;
        assert_eq!(raw.field(0).data_type(), &DataType::Struct(Fields::empty()));
        assert!(!is_interval_field(raw.field(0)));
        assert!(is_interval_field(raw.field(1)));
        assert_eq!(raw.field(1).data_type(), &DataType::FixedSizeBinary(12));
        Ok(())
    }
}
