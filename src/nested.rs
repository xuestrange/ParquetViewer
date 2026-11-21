//! Read-only tables over lossless Arrow cell JSON representations.
//!
//! The SQL type supplies field order and distinguishes MAP from STRUCT. JSON is
//! still a display representation: it cannot preserve original MAP key types,
//! duplicate JSON keys, or every scalar's original physical representation.
//! Rows are addressed on demand in the shared JSON tree, never materialized.
//!
//! Design references (checked 2026-10-30):
//! - https://parquet.apache.org/docs/file-format/nestedencoding/
//! - https://parquet.apache.org/docs/file-format/types/logicaltypes/#nested-types
//! - https://arrow.apache.org/docs/format/Columnar.html#struct-layout

use std::{collections::BTreeSet, io, io::Write, sync::Arc};

use serde_json::Value;

const JSON_PREVIEW_LIMIT: usize = 256 * 1024;
pub(crate) const JSON_PREVIEW_TRUNCATED: &str =
    "\n… JSON preview truncated. Copy Node or Save Value for the full JSON.";

/// Abort serialization at the display budget, before traversing later values.
/// Partial writes obey io::Write's contract; write_all retries the remainder
/// and receives the limit error as soon as the buffer has no space left.
struct JsonPreviewWriter {
    bytes: Vec<u8>,
    truncated: bool,
}

impl JsonPreviewWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(4096),
            truncated: false,
        }
    }

    fn into_preview(mut self) -> String {
        if self.truncated {
            self.bytes
                .truncate(JSON_PREVIEW_LIMIT - JSON_PREVIEW_TRUNCATED.len());
        }
        // A byte budget can stop inside a multibyte character. Keep only the
        // valid UTF-8 prefix, without cloning or building the complete JSON.
        let valid =
            std::str::from_utf8(&self.bytes).map_or_else(|error| error.valid_up_to(), str::len);
        self.bytes.truncate(valid);
        let mut preview = String::from_utf8(self.bytes)
            .expect("The JSON preview buffer was truncated to a valid UTF-8 prefix");
        if self.truncated {
            preview.push_str(JSON_PREVIEW_TRUNCATED);
        }
        preview
    }
}

impl Write for JsonPreviewWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = JSON_PREVIEW_LIMIT - self.bytes.len();
        if remaining == 0 {
            self.truncated = true;
            return Err(io::Error::other("JSON preview size limit reached"));
        }
        let copied = buffer.len().min(remaining);
        self.bytes.extend_from_slice(&buffer[..copied]);
        Ok(copied)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SqlType {
    pub label: String,
    kind: TypeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TypeKind {
    Unknown,
    Scalar,
    Struct(Vec<(String, SqlType)>),
    List(Box<SqlType>),
    Map(Box<SqlType>, Box<SqlType>),
}

impl SqlType {
    pub fn parse(source: &str) -> Self {
        let source = source.trim();
        let unknown = || Self {
            label: source.to_owned(),
            kind: TypeKind::Unknown,
        };
        if source.is_empty() {
            return unknown();
        }
        // Accept legacy LIST [] and explicit fixed-size ARRAY labels.
        if source.ends_with(']') {
            if let Some(start) = source.rfind('[') {
                let size = &source[start + 1..source.len() - 1];
                if size.is_empty() || size.parse::<usize>().is_ok() {
                    return Self {
                        label: source.to_owned(),
                        kind: TypeKind::List(Box::new(Self::parse(&source[..start]))),
                    };
                }
            }
        }
        let kind = if let Some(body) = type_body(source, "STRUCT") {
            let fields: Option<Vec<_>> = split_top_level(body).and_then(|parts| {
                parts
                    .into_iter()
                    .map(parse_field)
                    .collect::<Option<Vec<_>>>()
            });
            match fields {
                Some(fields) => TypeKind::Struct(fields),
                None => return unknown(),
            }
        } else if let Some(body) = type_body(source, "MAP") {
            match split_top_level(body) {
                Some(parts) if parts.len() == 2 => TypeKind::Map(
                    Box::new(Self::parse(parts[0])),
                    Box::new(Self::parse(parts[1])),
                ),
                _ => return unknown(),
            }
        } else if let Some(body) = type_body(source, "LIST") {
            TypeKind::List(Box::new(Self::parse(body)))
        } else {
            TypeKind::Scalar
        };
        Self {
            label: source.to_owned(),
            kind,
        }
    }

    pub fn is_nested(&self) -> bool {
        matches!(
            self.kind,
            TypeKind::Struct(_) | TypeKind::List(_) | TypeKind::Map(_, _)
        )
    }

    /// Borrow schema fields without exposing or reconstructing the parser's
    /// representation. Quoted field names have already been decoded exactly.
    pub fn struct_fields(&self) -> Option<&[(String, SqlType)]> {
        match &self.kind {
            TypeKind::Struct(fields) => Some(fields),
            _ => None,
        }
    }

    pub fn list_element(&self) -> Option<&SqlType> {
        match &self.kind {
            TypeKind::List(element) => Some(element),
            _ => None,
        }
    }

    fn unknown() -> Self {
        Self {
            label: "JSON".into(),
            kind: TypeKind::Unknown,
        }
    }
}

fn type_body<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    source
        .get(..name.len())?
        .eq_ignore_ascii_case(name)
        .then_some(())?;
    let rest = source[name.len()..].trim();
    rest.strip_prefix('(')?.strip_suffix(')')
}

/// Commas in DECIMAL, nested types, and quoted field names are not separators.
fn split_top_level(source: &str) -> Option<Vec<&str>> {
    if source.trim().is_empty() {
        return Some(Vec::new());
    }
    let mut depth = 0_usize;
    let mut quote = None;
    let mut start = 0;
    let mut parts = Vec::new();
    let mut chars = source.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if let Some(active) = quote {
            if ch == active {
                if chars.peek().is_some_and(|(_, next)| *next == active) {
                    chars.next();
                } else {
                    quote = None;
                }
            }
        } else {
            match ch {
                '"' | '\'' => quote = Some(ch),
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.checked_sub(1)?,
                ',' if depth == 0 => {
                    let part = source[start..index].trim();
                    if part.is_empty() {
                        return None;
                    }
                    parts.push(part);
                    start = index + ch.len_utf8();
                }
                _ => {}
            }
        }
    }
    if depth != 0 || quote.is_some() {
        return None;
    }
    let last = source[start..].trim();
    if last.is_empty() {
        return None;
    }
    parts.push(last);
    Some(parts)
}

fn parse_field(source: &str) -> Option<(String, SqlType)> {
    if let Some(body) = source.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = body.char_indices().peekable();
        while let Some((index, ch)) = chars.next() {
            if ch == '"' {
                if chars.peek().is_some_and(|(_, ch)| *ch == '"') {
                    chars.next();
                    name.push('"');
                } else {
                    let data_type = body[index + 1..].trim();
                    return (!data_type.is_empty()).then(|| (name, SqlType::parse(data_type)));
                }
            } else {
                name.push(ch);
            }
        }
        None
    } else {
        let boundary = source.find(char::is_whitespace)?;
        let data_type = source[boundary..].trim();
        (!data_type.is_empty()).then(|| (source[..boundary].to_owned(), SqlType::parse(data_type)))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NestedColumn {
    pub name: String,
    pub data_type: String,
    hint: SqlType,
}

#[derive(Clone, Debug)]
enum Step {
    Key(String),
    Index(usize),
}

#[derive(Clone, Debug)]
enum Layout {
    Object,
    Records,
    Values,
    Map(Vec<String>),
    Scalar,
}

/// A view into one immutable JSON tree. Descending and Back retain the tree;
/// neither operation clones an entire nested list or materializes table rows.
#[derive(Clone, Debug)]
pub(crate) struct NestedTable {
    root: Arc<Value>,
    path: Vec<Step>,
    hint: SqlType,
    layout: Layout,
    pub columns: Vec<NestedColumn>,
    pub label: String,
}

enum CellValue<'a> {
    Json(&'a Value),
    JsonKey(&'a str),
    Missing,
    NullRow,
}

/// Missing-value semantics come from the JSON tree/schema, never display text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MissingKind {
    NullValue,
    MissingField,
    NullRecord,
}

impl NestedTable {
    pub fn new(value: Value, data_type: &str, label: String) -> Self {
        Self::at(
            Arc::new(value),
            Vec::new(),
            SqlType::parse(data_type),
            label,
        )
    }

    fn at(root: Arc<Value>, path: Vec<Step>, hint: SqlType, label: String) -> Self {
        let value = resolve(&root, &path);
        let mut columns = Vec::new();
        let layout = match value {
            Value::Object(values) if matches!(hint.kind, TypeKind::Map(_, _)) => {
                let TypeKind::Map(key, item) = &hint.kind else {
                    unreachable!()
                };
                columns.push(column(
                    "key".into(),
                    SqlType {
                        label: format!("JSON key (declared {})", key.label),
                        kind: TypeKind::Scalar,
                    },
                ));
                columns.push(column("value".into(), (**item).clone()));
                Layout::Map(values.keys().cloned().collect())
            }
            Value::Object(values) => {
                columns = object_columns(values.keys(), &hint);
                if columns.is_empty() {
                    Layout::Scalar
                } else {
                    Layout::Object
                }
            }
            Value::Array(values) => {
                let item_hint = match &hint.kind {
                    TypeKind::List(item) => (**item).clone(),
                    _ => SqlType::unknown(),
                };
                let objects_only = values
                    .iter()
                    .all(|value| value.is_object() || value.is_null());
                let records = objects_only
                    && (matches!(item_hint.kind, TypeKind::Struct(_))
                        || values.iter().any(Value::is_object));
                if records {
                    let mut keys = BTreeSet::new();
                    for value in values {
                        if let Some(object) = value.as_object() {
                            keys.extend(object.keys());
                        }
                    }
                    columns = object_columns(keys.into_iter(), &item_hint);
                }
                if records && !columns.is_empty() {
                    Layout::Records
                } else {
                    columns.push(column("value".into(), item_hint));
                    Layout::Values
                }
            }
            _ => Layout::Scalar,
        };
        if columns.is_empty() {
            columns.push(column("value".into(), hint.clone()));
        }
        Self {
            root,
            path,
            hint,
            layout,
            columns,
            label,
        }
    }

    pub fn value(&self) -> &Value {
        resolve(&self.root, &self.path)
    }

    pub fn row_count(&self) -> usize {
        match &self.layout {
            Layout::Values | Layout::Records => self.value().as_array().map_or(0, Vec::len),
            Layout::Map(keys) => keys.len(),
            Layout::Object | Layout::Scalar => 1,
        }
    }

    pub fn summary(&self) -> String {
        let kind = match &self.layout {
            Layout::Object if matches!(self.hint.kind, TypeKind::Struct(_)) => "STRUCT",
            Layout::Object => "JSON object",
            Layout::Records if matches!(self.hint.kind, TypeKind::List(_)) => "LIST of records",
            Layout::Records => "JSON array of objects",
            Layout::Values if matches!(self.hint.kind, TypeKind::List(_)) => "LIST / ARRAY",
            Layout::Values => "JSON array (mixed values retained)",
            Layout::Map(_) => "MAP",
            Layout::Scalar if self.value().is_null() => "NULL value",
            Layout::Scalar if self.value().is_object() => "Empty object {}",
            Layout::Scalar => "Scalar value",
        };
        format!(
            "{kind} · {} rows × {} columns",
            self.row_count(),
            self.columns.len()
        )
    }

    pub fn representation_note(&self) -> &'static str {
        if matches!(self.layout, Layout::Map(_)) {
            "JSON preview: MAP keys are JSON strings; original key types and duplicate keys cannot be reconstructed."
        } else {
            "JSON view follows the field schema; scalar formatting and original physical types may differ."
        }
    }

    pub fn row_label(&self, row: usize) -> String {
        if matches!(self.layout, Layout::Records)
            && self.value().get(row).is_some_and(Value::is_null)
        {
            format!("{} · NULL", row + 1)
        } else {
            (row + 1).to_string()
        }
    }

    fn cell_value(&self, row: usize, column: usize) -> Option<CellValue<'_>> {
        if row >= self.row_count() || column >= self.columns.len() {
            return None;
        }
        let name = &self.columns[column].name;
        Some(match &self.layout {
            Layout::Object => self
                .value()
                .get(name)
                .map(CellValue::Json)
                .unwrap_or(CellValue::Missing),
            Layout::Records => {
                let record = self.value().get(row)?;
                if record.is_null() {
                    CellValue::NullRow
                } else {
                    record
                        .get(name)
                        .map(CellValue::Json)
                        .unwrap_or(CellValue::Missing)
                }
            }
            Layout::Values => CellValue::Json(self.value().get(row)?),
            Layout::Map(keys) if column == 0 => CellValue::JsonKey(&keys[row]),
            Layout::Map(keys) => CellValue::Json(self.value().get(&keys[row])?),
            Layout::Scalar => CellValue::Json(self.value()),
        })
    }

    /// Look up only the requested cell. Does not allocate, serialize a value,
    /// or scan any siblings/descendants. Out-of-range cells are not missing.
    pub fn cell_missing_kind(&self, row: usize, column: usize) -> Option<MissingKind> {
        match self.cell_value(row, column)? {
            CellValue::Json(Value::Null) => Some(MissingKind::NullValue),
            CellValue::Missing => Some(MissingKind::MissingField),
            CellValue::NullRow => Some(MissingKind::NullRecord),
            _ => None,
        }
    }

    pub fn cell_text(&self, row: usize, column: usize) -> Option<String> {
        Some(match self.cell_value(row, column)? {
            CellValue::Json(value) => value.to_string(),
            CellValue::JsonKey(key) => Value::String(key.to_owned()).to_string(),
            CellValue::Missing => "〈missing field〉".into(),
            CellValue::NullRow => "〈NULL record〉".into(),
        })
    }

    /// Compact visible text never serializes an entire child container.
    pub fn cell_preview(&self, row: usize, column: usize) -> Option<String> {
        match self.cell_value(row, column)? {
            CellValue::Json(Value::Object(values))
                if values.is_empty() && matches!(self.layout, Layout::Scalar) =>
            {
                Some("{}".into())
            }
            CellValue::Json(Value::Array(values)) => Some(format!("[{} items]  ›", values.len())),
            CellValue::Json(Value::Object(values)) => {
                Some(format!("{{{} fields}}  ›", values.len()))
            }
            CellValue::Json(Value::String(value)) => {
                let mut chars = value.chars();
                let prefix: String = chars.by_ref().take(240).collect();
                let mut text = Value::String(prefix).to_string();
                if chars.next().is_some() {
                    text.push('…');
                }
                Some(text)
            }
            _ => self.cell_text(row, column).map(|text| {
                let mut chars = text.chars();
                let mut clipped: String = chars.by_ref().take(240).collect();
                if chars.next().is_some() {
                    clipped.push('…');
                }
                clipped
            }),
        }
    }

    pub fn is_expandable(&self, row: usize, column: usize) -> bool {
        matches!(self.cell_value(row, column), Some(CellValue::Json(value))
            if value.is_object() || value.is_array())
            && !matches!(self.layout, Layout::Scalar)
    }

    pub fn open_cell(&self, row: usize, column: usize) -> Option<Self> {
        if !self.is_expandable(row, column) {
            return None;
        }
        let mut path = self.path.clone();
        let name = &self.columns[column].name;
        let label = match &self.layout {
            Layout::Object => {
                path.push(Step::Key(name.clone()));
                name.clone()
            }
            Layout::Records => {
                path.push(Step::Index(row));
                path.push(Step::Key(name.clone()));
                format!("[{}].{name}", row + 1)
            }
            Layout::Values => {
                path.push(Step::Index(row));
                format!("[{}]", row + 1)
            }
            Layout::Map(keys) => {
                path.push(Step::Key(keys[row].clone()));
                format!("[{}]", Value::String(keys[row].clone()))
            }
            Layout::Scalar => return None,
        };
        Some(Self::at(
            self.root.clone(),
            path,
            self.columns[column].hint.clone(),
            label,
        ))
    }

    pub fn node_json(&self) -> String {
        serde_json::to_string_pretty(self.value()).unwrap_or_else(|_| self.value().to_string())
    }

    /// Cached by the inspector when opening or navigating a node, never per
    /// render. Unlike Copy/Save's node_json, the display stops at 256 KiB total.
    pub fn node_json_preview(&self) -> String {
        let mut writer = JsonPreviewWriter::new();
        if serde_json::to_writer_pretty(&mut writer, self.value()).is_err() && !writer.truncated {
            return "JSON preview could not be generated. Copy Node or Save Value for the full JSON."
                .into();
        }
        writer.into_preview()
    }

    pub fn to_tsv(&self) -> Result<String, csv::Error> {
        let mut writer = csv::WriterBuilder::new()
            .delimiter(b'\t')
            .from_writer(Vec::new());
        writer.write_record(self.columns.iter().map(|column| column.name.as_str()))?;
        for row in 0..self.row_count() {
            writer.write_record(
                (0..self.columns.len())
                    .map(|column| self.cell_text(row, column).unwrap_or_default()),
            )?;
        }
        writer.flush()?;
        Ok(
            String::from_utf8(writer.into_inner().map_err(|error| error.into_error())?)
                .unwrap_or_default(),
        )
    }
}

fn column(name: String, hint: SqlType) -> NestedColumn {
    NestedColumn {
        name,
        data_type: hint.label.clone(),
        hint,
    }
}

fn object_columns<'a>(keys: impl Iterator<Item = &'a String>, hint: &SqlType) -> Vec<NestedColumn> {
    let mut extra: BTreeSet<&String> = keys.collect();
    let mut columns = Vec::new();
    if let TypeKind::Struct(fields) = &hint.kind {
        for (name, data_type) in fields {
            extra.remove(name);
            columns.push(column(name.clone(), data_type.clone()));
        }
    }
    columns.extend(
        extra
            .into_iter()
            .map(|name| column(name.clone(), SqlType::unknown())),
    );
    columns
}

fn resolve<'a>(root: &'a Value, path: &[Step]) -> &'a Value {
    let mut value = root;
    for step in path {
        value = match step {
            Step::Key(key) => value.get(key),
            Step::Index(index) => value.get(*index),
        }
        .unwrap_or(&Value::Null);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sql_hints_parse_nested_fields_quoted_names_and_array_dimensions() {
        let hint = SqlType::parse(
            "STRUCT(\"b, x\" DECIMAL(18, 4), \"quo\"\"te\" MAP(INTEGER, STRUCT(z VARCHAR)[]), a INTEGER[3])[]",
        );
        let TypeKind::List(item) = hint.kind else {
            panic!("expected list")
        };
        let TypeKind::Struct(fields) = item.kind else {
            panic!("expected struct")
        };
        assert_eq!(
            fields
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["b, x", "quo\"te", "a"]
        );
        assert_eq!(fields[0].1.label, "DECIMAL(18, 4)");
        assert!(matches!(fields[1].1.kind, TypeKind::Map(_, _)));
        assert!(matches!(fields[2].1.kind, TypeKind::List(_)));
        assert!(matches!(
            SqlType::parse("STRUCT(broken)").kind,
            TypeKind::Unknown
        ));
    }

    #[test]
    fn struct_is_one_row_in_schema_order_and_retains_null_empty_and_missing() {
        let table = NestedTable::new(
            json!({"a": null, "z": "", "extra": 7}),
            "STRUCT(z VARCHAR, a INTEGER, missing VARCHAR)",
            "root".into(),
        );
        assert_eq!(table.row_count(), 1);
        assert_eq!(
            table
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["z", "a", "missing", "extra"]
        );
        assert_eq!(table.cell_text(0, 0).as_deref(), Some("\"\""));
        assert_eq!(table.cell_text(0, 1).as_deref(), Some("null"));
        assert_eq!(table.cell_text(0, 2).as_deref(), Some("〈missing field〉"));
        assert_eq!(table.cell_text(1, 0), None);
    }

    #[test]
    fn missing_kinds_preserve_null_fields_absent_fields_and_null_records() {
        let table = NestedTable::new(
            json!([
                {"a": null, "literal": "NULL", "empty": ""},
                {"literal": "null", "empty": "NaN"},
                null
            ]),
            "STRUCT(a INTEGER, literal VARCHAR, empty VARCHAR)[]",
            "records".into(),
        );
        assert_eq!(table.cell_missing_kind(0, 0), Some(MissingKind::NullValue));
        assert_eq!(
            table.cell_missing_kind(1, 0),
            Some(MissingKind::MissingField)
        );
        for column in 0..3 {
            assert_eq!(
                table.cell_missing_kind(2, column),
                Some(MissingKind::NullRecord)
            );
        }
        for (row, column) in [(0, 1), (0, 2), (1, 1), (1, 2)] {
            assert_eq!(table.cell_missing_kind(row, column), None);
        }
        assert_eq!(table.cell_text(0, 0).as_deref(), Some("null"));
        assert_eq!(table.cell_text(0, 1).as_deref(), Some("\"NULL\""));
        assert_eq!(table.cell_text(1, 0).as_deref(), Some("〈missing field〉"));
        assert_eq!(table.cell_text(2, 0).as_deref(), Some("〈NULL record〉"));
        assert_eq!(table.cell_missing_kind(3, 0), None);
        assert_eq!(table.cell_missing_kind(0, 3), None);
    }

    #[test]
    fn list_values_do_not_treat_empty_or_null_like_literals_as_missing() {
        let table = NestedTable::new(
            json!([null, "", "NULL", "null", "NaN", 0, false, [], {}]),
            "JSON[]",
            "values".into(),
        );
        assert_eq!(table.cell_missing_kind(0, 0), Some(MissingKind::NullValue));
        for row in 1..table.row_count() {
            assert_eq!(table.cell_missing_kind(row, 0), None, "row {row}");
        }
        let scalar_null = NestedTable::new(Value::Null, "INTEGER[]", "null".into());
        assert_eq!(
            scalar_null.cell_missing_kind(0, 0),
            Some(MissingKind::NullValue)
        );
    }

    #[test]
    fn map_null_values_are_missing_but_null_like_keys_are_present() {
        let table = NestedTable::new(
            json!({"null": null, "NULL": "NULL", "NaN": "NaN", "empty": ""}),
            "MAP(VARCHAR, VARCHAR)",
            "map".into(),
        );
        for row in 0..table.row_count() {
            assert_eq!(table.cell_missing_kind(row, 0), None);
            let key = table.cell_text(row, 0).unwrap();
            assert_eq!(
                table.cell_missing_kind(row, 1),
                if key == "\"null\"" {
                    Some(MissingKind::NullValue)
                } else {
                    None
                },
                "key {key}"
            );
        }
    }

    #[test]
    fn records_recurse_without_copying_root_and_back_retains_exact_source() {
        let table = NestedTable::new(
            json!([{"name":"A", "items":[1,null,3]},null,{"name":"B","items":[]}]),
            "STRUCT(name VARCHAR, items INTEGER[])[]",
            "root".into(),
        );
        assert_eq!(table.row_count(), 3);
        assert_eq!(table.row_label(1), "2 · NULL");
        assert_eq!(table.cell_text(1, 0).as_deref(), Some("〈NULL record〉"));
        let child = table.open_cell(0, 1).expect("nested list");
        assert!(Arc::ptr_eq(&table.root, &child.root));
        assert_eq!(child.row_count(), 3);
        assert_eq!(child.cell_text(1, 0).as_deref(), Some("null"));
        assert_eq!(table.open_cell(2, 1).unwrap().row_count(), 0);
        assert_eq!(table.value()[0]["name"], "A");
    }

    #[test]
    fn maps_are_key_value_rows_and_do_not_invent_typed_json_keys() {
        let table = NestedTable::new(
            json!({"2": [2], "1": null}),
            "MAP(INTEGER, INTEGER[])",
            "root".into(),
        );
        assert_eq!(table.row_count(), 2);
        assert_eq!(table.columns[0].name, "key");
        assert_eq!(table.columns[1].name, "value");
        assert!(table.columns[0].data_type.contains("JSON key"));
        // serde_json's preserve_order feature can be enabled by another crate;
        // MAP row order is not part of this preview's type-fidelity contract.
        let null_row = (0..table.row_count())
            .find(|row| table.cell_text(*row, 0).as_deref() == Some("\"1\""))
            .unwrap();
        let list_row = (0..table.row_count())
            .find(|row| table.cell_text(*row, 0).as_deref() == Some("\"2\""))
            .unwrap();
        assert_eq!(table.cell_text(null_row, 1).as_deref(), Some("null"));
        assert!(
            table
                .representation_note()
                .contains("cannot be reconstructed")
        );
        assert_eq!(
            table
                .open_cell(list_row, 1)
                .unwrap()
                .cell_text(0, 0)
                .as_deref(),
            Some("2")
        );
        assert!(!table.is_expandable(1, 0));
    }

    #[test]
    fn null_empty_collections_and_empty_struct_are_distinct() {
        let null = NestedTable::new(Value::Null, "INTEGER[]", "root".into());
        let list = NestedTable::new(json!([]), "INTEGER[]", "root".into());
        let map = NestedTable::new(json!({}), "MAP(VARCHAR, INTEGER)", "root".into());
        let object = NestedTable::new(json!({}), "STRUCT()", "root".into());
        assert_eq!(null.row_count(), 1);
        assert_eq!(null.cell_text(0, 0).as_deref(), Some("null"));
        assert_eq!(list.row_count(), 0);
        assert_eq!(map.row_count(), 0);
        assert_eq!(object.row_count(), 1);
        assert_eq!(object.cell_text(0, 0).as_deref(), Some("{}"));
        let empty_records = NestedTable::new(
            json!([]),
            "STRUCT(id INTEGER, label VARCHAR)[]",
            "root".into(),
        );
        assert_eq!(empty_records.row_count(), 0);
        assert_eq!(empty_records.columns.len(), 2);
    }

    #[test]
    fn mixed_lists_keep_every_value_and_unknown_objects_are_not_claimed_structs() {
        let table = NestedTable::new(json!([{"a":1},2,null,[],""]), "JSON", "root".into());
        assert_eq!(table.row_count(), 5);
        assert_eq!(table.columns.len(), 1);
        assert_eq!(table.cell_text(1, 0).as_deref(), Some("2"));
        assert_eq!(table.cell_text(4, 0).as_deref(), Some("\"\""));
        let object = table.open_cell(0, 0).unwrap();
        assert!(object.summary().starts_with("JSON object"));
        assert_eq!(object.cell_text(0, 0).as_deref(), Some("1"));
    }

    #[test]
    fn large_lists_address_late_rows_without_materializing_or_serializing_children() {
        let values: Vec<_> = (0..100_000)
            .map(|index| json!({"id":index,"child":[index,index + 1]}))
            .collect();
        let table = NestedTable::new(
            Value::Array(values),
            "STRUCT(id INTEGER, child INTEGER[])[]",
            "root".into(),
        );
        assert_eq!(table.row_count(), 100_000);
        assert_eq!(table.cell_text(99_999, 0).as_deref(), Some("99999"));
        assert_eq!(
            table.cell_preview(99_999, 1).as_deref(),
            Some("[2 items]  ›")
        );
        assert_eq!(
            table
                .open_cell(99_999, 1)
                .unwrap()
                .cell_text(1, 0)
                .as_deref(),
            Some("100000")
        );
        assert_eq!(Arc::strong_count(&table.root), 1);
    }

    #[test]
    fn table_copy_quotes_headers_and_keeps_full_values() -> anyhow::Result<()> {
        let table = NestedTable::new(
            json!([{"line\tname":"a\nb", "value":null}]),
            "",
            "root".into(),
        );
        let tsv = table.to_tsv()?;
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .from_reader(tsv.as_bytes());
        assert_eq!(reader.headers()?.get(0), Some("line\tname"));
        let record = reader.records().next().unwrap()?;
        assert_eq!(record.get(0), Some("\"a\\nb\""));
        assert_eq!(record.get(1), Some("null"));
        Ok(())
    }

    #[test]
    fn huge_integer_and_long_decimal_keep_exact_text_in_cells_nodes_and_table_copy()
    -> anyhow::Result<()> {
        // Parsing decimal text (rather than json! with Rust numeric literals)
        // catches a regression to serde_json's default f64 conversion.
        let huge = "170141183460469231731687303715884105727";
        let decimal = "1234567890.1234567890123456789012345678";
        let json = format!("[{{\"id\":{huge},\"amount\":{decimal}}}]");
        let table = NestedTable::new(
            serde_json::from_str(&json)?,
            "STRUCT(id HUGEINT, amount DECIMAL(38,28))[]",
            "precision".into(),
        );
        assert_eq!(table.cell_text(0, 0).as_deref(), Some(huge));
        assert_eq!(table.cell_text(0, 1).as_deref(), Some(decimal));
        let node = table.node_json();
        assert!(node.contains(&format!("\"id\": {huge}")));
        assert!(node.contains(&format!("\"amount\": {decimal}")));
        let copy = table.to_tsv()?;
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .from_reader(copy.as_bytes());
        assert_eq!(
            reader.headers()?.iter().collect::<Vec<_>>(),
            ["id", "amount"]
        );
        let row = reader.records().next().unwrap()?;
        assert_eq!(row.get(0), Some(huge));
        assert_eq!(row.get(1), Some(decimal));
        Ok(())
    }

    #[test]
    fn node_json_preview_is_pretty_exact_and_follows_navigation() -> anyhow::Result<()> {
        let huge = "170141183460469231731687303715884105727";
        let decimal = "1234567890.1234567890123456789012345678";
        let source = format!(
            r#"{{"label":"中文😀","children":[{{"id":{huge},"amount":{decimal}}},null],"empty":[]}}"#
        );
        let table = NestedTable::new(
            serde_json::from_str(&source)?,
            "STRUCT(label VARCHAR, children STRUCT(id HUGEINT, amount DECIMAL(38,28))[], empty INTEGER[])",
            "root".into(),
        );
        let root_preview = table.node_json_preview();
        assert_eq!(root_preview, table.node_json());
        assert_eq!(
            serde_json::from_str::<Value>(&root_preview)?,
            *table.value()
        );
        assert!(root_preview.contains("\n  \"label\": \"中文😀\""));
        assert!(root_preview.contains(&format!("\"id\": {huge}")));
        assert!(root_preview.contains(&format!("\"amount\": {decimal}")));

        let children = table
            .columns
            .iter()
            .position(|column| column.name == "children")
            .unwrap();
        let child = table.open_cell(0, children).unwrap();
        let child_preview = child.node_json_preview();
        assert_eq!(child_preview, child.node_json());
        assert_eq!(
            serde_json::from_str::<Value>(&child_preview)?,
            table.value()["children"]
        );
        assert!(child_preview.starts_with("[\n  {\n"));
        assert!(!child_preview.contains("中文😀"));
        assert!(Arc::ptr_eq(&table.root, &child.root));
        // Back reuses the root view; its preview is still the exact root node.
        assert_eq!(table.node_json_preview(), root_preview);
        let null = NestedTable::new(Value::Null, "INTEGER[]", "null".into());
        assert_eq!(null.node_json_preview(), "null");
        Ok(())
    }

    #[test]
    fn large_node_json_preview_is_bounded_valid_unicode_and_copy_remains_complete() {
        let mut values = vec![Value::String("中文😀".repeat(128)); 1000];
        values.push(Value::String("TAIL_NOT_INCLUDED_IN_PREVIEW".into()));
        let table = NestedTable::new(Value::Array(values), "VARCHAR[]", "large".into());
        let preview = table.node_json_preview();
        assert!(preview.len() <= JSON_PREVIEW_LIMIT);
        assert!(preview.ends_with(JSON_PREVIEW_TRUNCATED));
        assert!(!preview.contains("TAIL_NOT_INCLUDED_IN_PREVIEW"));
        assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
        assert!(!preview.contains('\u{fffd}'));
        let full = table.node_json();
        assert!(full.len() > JSON_PREVIEW_LIMIT);
        assert!(full.contains("TAIL_NOT_INCLUDED_IN_PREVIEW"));
        assert!(full.starts_with(preview.strip_suffix(JSON_PREVIEW_TRUNCATED).unwrap()));
    }

    #[test]
    fn bounded_json_writer_aborts_before_serializing_later_list_elements() {
        use serde::{Serialize, Serializer, ser::SerializeSeq};
        use std::cell::Cell;

        struct CountedList<'a> {
            visited: &'a Cell<usize>,
            value: &'a str,
        }
        impl Serialize for CountedList<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut sequence = serializer.serialize_seq(Some(100_000))?;
                for _ in 0..100_000 {
                    self.visited.set(self.visited.get() + 1);
                    sequence.serialize_element(self.value)?;
                }
                sequence.end()
            }
        }
        let visited = Cell::new(0);
        let value = "中文😀".repeat(128);
        let values = CountedList {
            visited: &visited,
            value: &value,
        };
        let mut writer = JsonPreviewWriter::new();
        assert!(serde_json::to_writer_pretty(&mut writer, &values).is_err());
        assert!(writer.truncated);
        assert_eq!(writer.bytes.len(), JSON_PREVIEW_LIMIT);
        assert!(visited.get() < 1000, "Visited {} elements", visited.get());
        let preview = writer.into_preview();
        assert!(preview.len() <= JSON_PREVIEW_LIMIT);
        assert!(preview.ends_with(JSON_PREVIEW_TRUNCATED));
        assert!(!preview.contains('\u{fffd}'));
    }
}
