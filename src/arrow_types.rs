//! Stable presentation labels for Arrow types. These are UI/schema notation,
//! independent of any database engine; values keep their original Arrow type.
use crate::engine::ColumnInfo;
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};

pub fn columns_from_schema(schema: &Schema) -> Vec<ColumnInfo> {
    schema
        .fields()
        .iter()
        .map(|field| ColumnInfo {
            name: field.name().clone(),
            data_type: field_type_label(field),
            nullable: field.is_nullable(),
        })
        .collect()
}

/// Canonical extensions belong to Fields, including children of containers.
/// Inspecting only a DataType would erase JSON/UUID hints in nested values.
pub fn field_type_label(field: &Field) -> String {
    if let Some(label) = crate::parquet_compat::field_label(field) {
        return label.into();
    }
    match field
        .metadata()
        .get("ARROW:extension:name")
        .map(String::as_str)
    {
        Some("arrow.json") => "JSON".into(),
        Some("arrow.uuid") => "UUID".into(),
        _ => sql_type_label(field.data_type()),
    }
}

pub fn sql_type_label(kind: &DataType) -> String {
    match kind {
        DataType::Null => "NULL".into(),
        DataType::Boolean => "BOOLEAN".into(),
        DataType::Int8 => "TINYINT".into(),
        DataType::Int16 => "SMALLINT".into(),
        DataType::Int32 => "INTEGER".into(),
        DataType::Int64 => "INT64".into(),
        DataType::UInt8 => "UTINYINT".into(),
        DataType::UInt16 => "USMALLINT".into(),
        DataType::UInt32 => "UINTEGER".into(),
        DataType::UInt64 => "UBIGINT".into(),
        DataType::Float16 | DataType::Float32 => "FLOAT".into(),
        DataType::Float64 => "DOUBLE".into(),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "VARCHAR".into(),
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => "BLOB".into(),
        DataType::Date32 | DataType::Date64 => "DATE".into(),
        DataType::Timestamp(unit, Some(_)) => match unit {
            TimeUnit::Second => "TIMESTAMP(0) WITH TIME ZONE",
            TimeUnit::Millisecond => "TIMESTAMP(3) WITH TIME ZONE",
            TimeUnit::Microsecond => "TIMESTAMP WITH TIME ZONE",
            TimeUnit::Nanosecond => "TIMESTAMP(9) WITH TIME ZONE",
        }
        .into(),
        DataType::Timestamp(unit, None) => match unit {
            TimeUnit::Second => "TIMESTAMP_S",
            TimeUnit::Millisecond => "TIMESTAMP_MS",
            TimeUnit::Microsecond => "TIMESTAMP",
            TimeUnit::Nanosecond => "TIMESTAMP_NS",
        }
        .into(),
        DataType::Time32(unit) | DataType::Time64(unit) => if *unit == TimeUnit::Nanosecond {
            "TIME_NS"
        } else {
            "TIME"
        }
        .into(),
        DataType::Duration(_) | DataType::Interval(_) => "INTERVAL".into(),
        DataType::Decimal32(p, s)
        | DataType::Decimal64(p, s)
        | DataType::Decimal128(p, s)
        | DataType::Decimal256(p, s) => format!("DECIMAL({p},{s})"),
        DataType::Struct(fields) => format!(
            "STRUCT({})",
            fields
                .iter()
                .map(|field| format!(
                    "\"{}\" {}",
                    field.name().replace('"', "\"\""),
                    field_type_label(field)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        DataType::List(field)
        | DataType::LargeList(field)
        | DataType::ListView(field)
        | DataType::LargeListView(field) => format!("{}[]", field_type_label(field)),
        DataType::FixedSizeList(field, count) => {
            format!("{}[{count}]", field_type_label(field))
        }
        DataType::Map(field, _) => match field.data_type() {
            DataType::Struct(fields) if fields.len() == 2 => format!(
                "MAP({}, {})",
                field_type_label(&fields[0]),
                field_type_label(&fields[1])
            ),
            _ => format!("MAP({})", field_type_label(field)),
        },
        DataType::Dictionary(_, value) => sql_type_label(value),
        DataType::RunEndEncoded(_, value) => field_type_label(value),
        DataType::Union(fields, _) => format!(
            "UNION({})",
            fields
                .iter()
                .map(|(_, field)| format!(
                    "\"{}\" {}",
                    field.name().replace('"', "\"\""),
                    field_type_label(field)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, ExportFormat, QueryRequest};
    use anyhow::Result;
    use arrow::{
        array::{
            Array, ArrayRef, FixedSizeBinaryArray, ListArray, MapArray, StringArray, StructArray,
        },
        buffer::{NullBuffer, OffsetBuffer},
        datatypes::SchemaRef,
        record_batch::RecordBatch,
    };
    use parquet::{
        arrow::{ArrowWriter, arrow_writer::ArrowWriterOptions},
        basic::LogicalType,
        file::reader::{FileReader, SerializedFileReader},
    };
    use serde_json::{Value, json};
    use std::{fs::File, path::Path, sync::Arc};

    fn extension_field(name: &str, kind: DataType, extension: &str) -> Field {
        let mut metadata = [("ARROW:extension:name".into(), extension.into())]
            .into_iter()
            .collect::<std::collections::HashMap<String, String>>();
        if extension == "arrow.json" {
            metadata.insert("ARROW:extension:metadata".into(), String::new());
        }
        Field::new(name, kind, true).with_metadata(metadata)
    }

    fn canonical_fixture() -> Result<RecordBatch> {
        let json_field = extension_field("json", DataType::Utf8, "arrow.json");
        let uuid_field = extension_field("uuid", DataType::FixedSizeBinary(16), "arrow.uuid");
        let json = Arc::new(StringArray::from(vec![
            Some(r#"{"n":9007199254740993,"keep":[true,null]}"#),
            None,
            Some(r#""hello""#),
        ])) as ArrayRef;
        let uuid_bytes: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let reverse_uuid: [u8; 16] = [
            0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22,
            0x11, 0x00,
        ];
        let uuid = Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            [
                Some(uuid_bytes.as_slice()),
                None,
                Some(reverse_uuid.as_slice()),
            ]
            .into_iter(),
            16,
        )?) as ArrayRef;
        let record = Arc::new(StructArray::try_new(
            vec![Arc::new(json_field.clone()), Arc::new(uuid_field.clone())].into(),
            vec![json.clone(), uuid.clone()],
            Some(NullBuffer::from(vec![true, false, true])),
        )?) as ArrayRef;
        let list = |field: Field, values: ArrayRef| -> Result<ArrayRef> {
            Ok(Arc::new(ListArray::try_new(
                Arc::new(field.with_name("element")),
                OffsetBuffer::new(vec![0i32, 2, 2, 3].into()),
                values,
                Some(NullBuffer::from(vec![true, false, true])),
            )?))
        };
        let map = |field: Field, values: ArrayRef| -> Result<ArrayRef> {
            let entries = StructArray::try_new(
                vec![
                    Arc::new(Field::new("key", DataType::Utf8, false)),
                    Arc::new(field.with_name("value")),
                ]
                .into(),
                vec![
                    Arc::new(StringArray::from(vec!["first", "missing", "last"])),
                    values,
                ],
                None,
            )?;
            Ok(Arc::new(MapArray::try_new(
                Arc::new(Field::new("entries", entries.data_type().clone(), false)),
                OffsetBuffer::new(vec![0i32, 2, 2, 3].into()),
                entries,
                Some(NullBuffer::from(vec![true, false, true])),
                false,
            )?))
        };
        let documents = list(json_field.clone(), json.clone())?;
        let identifiers = list(uuid_field.clone(), uuid.clone())?;
        let documents_by_name = map(json_field.clone(), json.clone())?;
        let identifiers_by_name = map(uuid_field.clone(), uuid.clone())?;
        let uuid_keys = Arc::new(FixedSizeBinaryArray::try_from_iter(
            [uuid_bytes.as_slice(), reverse_uuid.as_slice()].into_iter(),
        )?) as ArrayRef;
        let entries = StructArray::try_new(
            vec![
                Arc::new(uuid_field.clone().with_name("key").with_nullable(false)),
                Arc::new(json_field.clone().with_name("value")),
            ]
            .into(),
            vec![
                uuid_keys,
                Arc::new(StringArray::from(vec![
                    r#"{"n":9007199254740993,"keep":[true,null]}"#,
                    r#""hello""#,
                ])),
            ],
            None,
        )?;
        let documents_by_id = Arc::new(MapArray::try_new(
            Arc::new(Field::new("entries", entries.data_type().clone(), false)),
            OffsetBuffer::new(vec![0i32, 1, 1, 2].into()),
            entries,
            Some(NullBuffer::from(vec![true, false, true])),
            false,
        )?) as ArrayRef;
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            json_field.with_name("top_json"),
            uuid_field.with_name("top_uuid"),
            Field::new("record", record.data_type().clone(), true),
            Field::new("documents", documents.data_type().clone(), true),
            Field::new("identifiers", identifiers.data_type().clone(), true),
            Field::new(
                "documents_by_name",
                documents_by_name.data_type().clone(),
                true,
            ),
            Field::new(
                "identifiers_by_name",
                identifiers_by_name.data_type().clone(),
                true,
            ),
            Field::new("documents_by_id", documents_by_id.data_type().clone(), true),
        ]));
        Ok(RecordBatch::try_new(
            schema,
            vec![
                json,
                uuid,
                record,
                documents,
                identifiers,
                documents_by_name,
                identifiers_by_name,
                documents_by_id,
            ],
        )?)
    }

    fn check_logical_annotations(path: &Path, embedded_schema: bool) -> Result<()> {
        let reader = SerializedFileReader::new(File::open(path)?)?;
        let metadata = reader.metadata().file_metadata();
        assert_eq!(
            metadata
                .key_value_metadata()
                .is_some_and(|items| items.iter().any(|item| item.key == "ARROW:schema")),
            embedded_schema,
        );
        let mut json = 0;
        let mut uuid = 0;
        for column in metadata.schema_descr().columns() {
            match column.self_type().get_basic_info().logical_type_ref() {
                Some(LogicalType::Json) => json += 1,
                Some(LogicalType::Uuid) => uuid += 1,
                _ => {}
            }
        }
        assert_eq!(
            (json, uuid),
            (5, 5),
            "logical annotations must survive without Arrow schema hints"
        );
        Ok(())
    }

    #[test]
    fn parquet_canonical_json_uuid_without_arrow_schema_read_and_export() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("canonical.parquet");
        let batch = canonical_fixture()?;
        let expected_columns = columns_from_schema(&batch.schema());
        assert_eq!(
            expected_columns
                .iter()
                .map(|column| column.data_type.as_str())
                .collect::<Vec<_>>(),
            [
                "JSON",
                "UUID",
                "STRUCT(\"json\" JSON, \"uuid\" UUID)",
                "JSON[]",
                "UUID[]",
                "MAP(VARCHAR, JSON)",
                "MAP(VARCHAR, UUID)",
                "MAP(UUID, JSON)"
            ],
        );
        let mut writer = ArrowWriter::try_new_with_options(
            File::create_new(&source)?,
            batch.schema(),
            ArrowWriterOptions::new().with_skip_arrow_metadata(true),
        )?;
        writer.write(&batch)?;
        writer.close()?;
        check_logical_annotations(&source, false)?;

        let mut engine = Engine::open(&[source])?;
        assert_eq!(engine.info.columns, expected_columns);
        let result = engine.query(&QueryRequest {
            sort: Some(("top_uuid".into(), true)),
            ..Default::default()
        })?;
        assert_eq!(result.columns, expected_columns);
        assert_eq!(
            (0..3)
                .map(|row| result.data.source_row_number(row))
                .collect::<Vec<_>>(),
            [Some(3), Some(1), Some(2)]
        );
        assert_eq!(
            result.data.cell(1, 1).text,
            "00112233-4455-6677-8899-aabbccddeeff"
        );
        assert_eq!(
            result.data.cell(0, 1).text,
            "ffeeddcc-bbaa-9988-7766-554433221100"
        );
        assert!(result.data.is_null(2, 0));
        assert!(result.data.is_null(2, 2));
        let schema = result.data.schema();
        assert_eq!(
            schema
                .field(0)
                .metadata()
                .get("ARROW:extension:name")
                .map(String::as_str),
            Some("arrow.json")
        );
        assert_eq!(
            schema
                .field(1)
                .metadata()
                .get("ARROW:extension:name")
                .map(String::as_str),
            Some("arrow.uuid")
        );

        let json_path = directory.path().join("canonical.json");
        engine.export_result(&result.snapshot_id, &json_path, ExportFormat::Json)?;
        let actual: Value = serde_json::from_reader(File::open(&json_path)?)?;
        let object = json!({"n": 9007199254740993u64, "keep": [true, null]});
        let first_uuid = "00112233-4455-6677-8899-aabbccddeeff";
        let last_uuid = "ffeeddcc-bbaa-9988-7766-554433221100";
        assert_eq!(actual[0]["top_json"], "hello");
        assert_eq!(
            actual[0]["record"],
            json!({"json":"hello", "uuid":last_uuid})
        );
        assert_eq!(actual[0]["documents"], json!(["hello"]));
        assert_eq!(actual[0]["identifiers"], json!([last_uuid]));
        assert_eq!(actual[0]["documents_by_name"], json!({"last":"hello"}));
        assert_eq!(actual[0]["identifiers_by_name"], json!({"last":last_uuid}));
        assert_eq!(actual[0]["documents_by_id"][last_uuid], "hello");
        assert_eq!(actual[1]["top_json"], object);
        assert_eq!(
            actual[1]["record"],
            json!({"json":object, "uuid":first_uuid})
        );
        assert_eq!(actual[1]["documents"], json!([object, null]));
        assert_eq!(actual[1]["identifiers"], json!([first_uuid, null]));
        assert_eq!(
            actual[1]["documents_by_name"],
            json!({"first":object,"missing":null})
        );
        assert_eq!(
            actual[1]["identifiers_by_name"],
            json!({"first":first_uuid,"missing":null})
        );
        assert_eq!(actual[1]["documents_by_id"][first_uuid], object);
        assert!(actual[2].as_object().unwrap().values().all(Value::is_null));

        let parquet_path = directory.path().join("canonical-export.parquet");
        engine.export_result(&result.snapshot_id, &parquet_path, ExportFormat::Parquet)?;
        check_logical_annotations(&parquet_path, true)?;
        let mut roundtrip = Engine::open(&[parquet_path])?;
        let reopened = roundtrip.query(&QueryRequest::default())?;
        assert_eq!(reopened.columns, expected_columns);
        let roundtrip_json = directory.path().join("canonical-roundtrip.json");
        roundtrip.export_result(&reopened.snapshot_id, &roundtrip_json, ExportFormat::Json)?;
        let actual_roundtrip: Value = serde_json::from_reader(File::open(&roundtrip_json)?)?;
        assert_eq!(actual_roundtrip, actual);
        Ok(())
    }
}
