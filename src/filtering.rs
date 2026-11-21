//! Typed, schema-bound filters for the visual editor.
//!
//! This module accepts values, never SQL fragments. It deliberately exposes only
//! schema-validated Struct paths and complete primitive List elements. Scalar
//! text matching is case-insensitive and literal; List membership uses complete,
//! exact typed elements. Binary and unknown values remain NULL-only.

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

use crate::engine::ColumnInfo;
use crate::nested::SqlType;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchMode {
    #[default]
    All,
    Any,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterGroup {
    pub mode: MatchMode,
    pub conditions: Vec<FilterCondition>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterCondition {
    pub field: String,
    /// Exact Struct field segments; a literal dot in a field name is not a path.
    pub path: Vec<String>,
    pub operator: FilterOperator,
    pub value: String,
    pub second_value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterTarget {
    pub field: String,
    pub path: Vec<String>,
    pub data_type: String,
    pub label: String,
    /// For Lists, the element's physical type. Otherwise the target's type.
    pub value_type: String,
    pub is_list: bool,
}

/// Enumerate roots and Struct descendants in schema order, without reading any
/// dataset values. List<Struct> elements are not scalar Struct paths.
pub fn targets(columns: &[ColumnInfo]) -> Vec<FilterTarget> {
    let mut targets = Vec::new();
    for column in columns {
        let parsed = SqlType::parse(&column.data_type);
        let mut stack = vec![(Vec::new(), &parsed)];
        while let Some((path, kind)) = stack.pop() {
            let label = target_label(&column.name, &path);
            let element = kind.list_element();
            targets.push(FilterTarget {
                field: column.name.clone(),
                path: path.clone(),
                data_type: kind.label.clone(),
                label,
                value_type: element.unwrap_or(kind).label.clone(),
                is_list: element.is_some(),
            });
            if let Some(fields) = kind.struct_fields() {
                for (name, child) in fields.iter().rev() {
                    let mut child_path = path.clone();
                    child_path.push(name.clone());
                    stack.push((child_path, child));
                }
            }
        }
    }
    targets
}

fn target_label(field: &str, path: &[String]) -> String {
    if path.is_empty() {
        return field.to_owned();
    }
    std::iter::once(field)
        .chain(path.iter().map(String::as_str))
        .map(identifier)
        .collect::<Vec<_>>()
        .join(".")
}

pub fn list_element_type(data_type: &str) -> Option<String> {
    SqlType::parse(data_type)
        .list_element()
        .map(|element| element.label.clone())
}

pub fn value_kind(data_type: &str) -> FilterFieldKind {
    list_element_type(data_type)
        .map_or_else(|| field_kind(data_type), |element| field_kind(&element))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterOperator {
    #[default]
    Eq,
    NotEq,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Gt,
    Ge,
    Lt,
    Le,
    Between,
    IsNull,
    IsNotNull,
    IsEmpty,
    IsNotEmpty,
}

/// The editor and SQL validation share one schema classification. A value that
/// happens to look numeric or like a date never changes a text field's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterFieldKind {
    Number,
    Text,
    Boolean,
    Date,
    DateTime,
    Time,
    List,
    NullOnly,
}

impl FilterFieldKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Number => "Number",
            Self::Text => "Text",
            Self::Boolean => "Boolean",
            Self::Date => "Date",
            Self::DateTime => "Date & time",
            Self::Time => "Time",
            Self::List => "List",
            Self::NullOnly => "NULL checks only",
        }
    }
}

pub fn field_kind(data_type: &str) -> FilterFieldKind {
    if SqlType::parse(data_type).list_element().is_some() {
        return FilterFieldKind::List;
    }
    match FieldType::parse(data_type) {
        FieldType::Text => FilterFieldKind::Text,
        FieldType::Boolean => FilterFieldKind::Boolean,
        FieldType::Number(_) => FilterFieldKind::Number,
        FieldType::Temporal(TemporalType::Date) => FilterFieldKind::Date,
        FieldType::Temporal(
            TemporalType::Timestamp { .. } | TemporalType::ZonedTimestamp { .. },
        ) => FilterFieldKind::DateTime,
        FieldType::Temporal(TemporalType::Time { .. } | TemporalType::ZonedTime { .. }) => {
            FilterFieldKind::Time
        }
        FieldType::NullOnly => FilterFieldKind::NullOnly,
    }
}

/// Human temporal labels do not change the underlying, precision-preserving
/// comparisons. Timestamp equality still compares a complete instant/value,
/// rather than silently truncating every value to its calendar day.
pub fn operator_label(operator: FilterOperator, data_type: &str) -> &'static str {
    use FilterOperator::*;
    match (field_kind(data_type), operator) {
        (FilterFieldKind::List, Contains) => "Contains value",
        (FilterFieldKind::List, NotContains) => "Does not contain value",
        (FilterFieldKind::List, IsEmpty) => "Empty List",
        (FilterFieldKind::List, IsNotEmpty) => "Nonempty List",
        (FilterFieldKind::Date, Eq) => "On date",
        (FilterFieldKind::Date, NotEq) => "Not on date",
        (FilterFieldKind::Date, Ge) => "On or after",
        (FilterFieldKind::Date, Le) => "On or before",
        (FilterFieldKind::Date, Between) => "Between dates (inclusive)",
        (FilterFieldKind::DateTime, Eq) => "At date/time",
        (FilterFieldKind::DateTime, NotEq) => "Not at date/time",
        (FilterFieldKind::DateTime, Between) => "Between date/times (inclusive)",
        (FilterFieldKind::Time, Eq) => "At time",
        (FilterFieldKind::Time, NotEq) => "Not at time",
        (FilterFieldKind::Time, Between) => "Between times (inclusive)",
        (FilterFieldKind::DateTime | FilterFieldKind::Time, Ge) => "At or after",
        (FilterFieldKind::DateTime | FilterFieldKind::Time, Le) => "At or before",
        (FilterFieldKind::Date | FilterFieldKind::DateTime | FilterFieldKind::Time, Gt) => "After",
        (FilterFieldKind::Date | FilterFieldKind::DateTime | FilterFieldKind::Time, Lt) => "Before",
        _ => operator.label(),
    }
}

/// Untranslated placeholder keys/patterns for the concrete physical type.
pub fn value_placeholder(data_type: &str, upper: bool) -> &'static str {
    let element = list_element_type(data_type);
    let data_type = element.as_deref().unwrap_or(data_type);
    match FieldType::parse(data_type) {
        FieldType::Temporal(TemporalType::Date) => "YYYY-MM-DD",
        FieldType::Temporal(TemporalType::ZonedTimestamp { .. }) => "YYYY-MM-DD HH:MM:SS+HH:MM",
        FieldType::Temporal(TemporalType::Timestamp { .. }) => "YYYY-MM-DD HH:MM:SS",
        FieldType::Temporal(TemporalType::ZonedTime { .. }) => "HH:MM:SS+HH:MM",
        FieldType::Temporal(TemporalType::Time { .. }) => "HH:MM:SS",
        FieldType::Boolean => "true / false",
        FieldType::Number(_) if !upper => "Number",
        _ if upper => "Upper bound",
        _ => "Value",
    }
}

impl FilterOperator {
    pub fn label(self) -> &'static str {
        match self {
            Self::Eq => "Equals",
            Self::NotEq => "Does not equal",
            Self::Contains => "Contains",
            Self::NotContains => "Does not contain",
            Self::StartsWith => "Starts with",
            Self::EndsWith => "Ends with",
            Self::Gt => "Greater than",
            Self::Ge => "Greater than or equal",
            Self::Lt => "Less than",
            Self::Le => "Less than or equal",
            Self::Between => "Between (inclusive)",
            Self::IsNull => "Is NULL",
            Self::IsNotNull => "Is not NULL",
            Self::IsEmpty => "Is empty",
            Self::IsNotEmpty => "Is not empty",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(
            self,
            Self::IsNull | Self::IsNotNull | Self::IsEmpty | Self::IsNotEmpty
        )
    }
}

const NULL_OPERATORS: &[FilterOperator] = &[FilterOperator::IsNull, FilterOperator::IsNotNull];
const LIST_OPERATORS: &[FilterOperator] = &[
    FilterOperator::Contains,
    FilterOperator::NotContains,
    FilterOperator::IsEmpty,
    FilterOperator::IsNotEmpty,
    FilterOperator::IsNull,
    FilterOperator::IsNotNull,
];
const NESTED_LIST_OPERATORS: &[FilterOperator] = &[
    FilterOperator::IsEmpty,
    FilterOperator::IsNotEmpty,
    FilterOperator::IsNull,
    FilterOperator::IsNotNull,
];
const BOOLEAN_OPERATORS: &[FilterOperator] = &[
    FilterOperator::Eq,
    FilterOperator::NotEq,
    FilterOperator::IsNull,
    FilterOperator::IsNotNull,
];
const ORDERED_OPERATORS: &[FilterOperator] = &[
    FilterOperator::Eq,
    FilterOperator::NotEq,
    FilterOperator::Gt,
    FilterOperator::Ge,
    FilterOperator::Lt,
    FilterOperator::Le,
    FilterOperator::Between,
    FilterOperator::IsNull,
    FilterOperator::IsNotNull,
];
const TEXT_OPERATORS: &[FilterOperator] = &[
    FilterOperator::Eq,
    FilterOperator::NotEq,
    FilterOperator::Contains,
    FilterOperator::NotContains,
    FilterOperator::StartsWith,
    FilterOperator::EndsWith,
    FilterOperator::IsNull,
    FilterOperator::IsNotNull,
    FilterOperator::IsEmpty,
    FilterOperator::IsNotEmpty,
];

/// Operators supported by the visual editor. Unknown types stay conservative.
pub fn operators(data_type: &str) -> &'static [FilterOperator] {
    if let Some(element) = list_element_type(data_type) {
        return if matches!(FieldType::parse(&element), FieldType::NullOnly) {
            NESTED_LIST_OPERATORS
        } else {
            LIST_OPERATORS
        };
    }
    match FieldType::parse(data_type) {
        FieldType::Text => TEXT_OPERATORS,
        FieldType::Boolean => BOOLEAN_OPERATORS,
        FieldType::Number(_) | FieldType::Temporal(_) => ORDERED_OPERATORS,
        FieldType::NullOnly => NULL_OPERATORS,
    }
}

impl FilterGroup {
    /// Build one expression for `QueryRequest.filter` over the loaded window.
    /// Each condition is checked against the exact, currently selected schema.
    pub fn to_filter(&self, columns: &[ColumnInfo]) -> Result<String> {
        let mut expressions = Vec::with_capacity(self.conditions.len());
        for (index, condition) in self.conditions.iter().enumerate() {
            expressions.push(condition.to_expression(columns).with_context(|| {
                format!(
                    "Condition {} · field {:?}",
                    index + 1,
                    target_label(&condition.field, &condition.path)
                )
            })?);
        }
        if expressions.is_empty() {
            return Ok(String::new());
        }
        let join = match self.mode {
            MatchMode::All => " AND ",
            MatchMode::Any => " OR ",
        };
        Ok(format!("({})", expressions.join(join)))
    }
}

impl FilterCondition {
    fn to_expression(&self, columns: &[ColumnInfo]) -> Result<String> {
        let mut matches = columns.iter().filter(|column| column.name == self.field);
        let column = matches.next().context("Choose an available field.")?;
        ensure!(matches.next().is_none(), "The field name is ambiguous.");
        ensure!(
            !self.field.contains('\0'),
            "A field name cannot contain NUL."
        );
        let mut field = identifier(&self.field);
        let parsed = SqlType::parse(&column.data_type);
        let mut target = &parsed;
        for segment in &self.path {
            ensure!(!segment.contains('\0'), "A field name cannot contain NUL.");
            let fields = target
                .struct_fields()
                .context("Choose an available Struct field.")?;
            let mut matches = fields
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(segment));
            let (name, next) = matches
                .next()
                .context("Choose an available Struct field.")?;
            ensure!(
                matches.next().is_none(),
                "The Struct field name is ambiguous."
            );
            ensure!(name == segment, "Choose an available Struct field.");
            target = next;
            field = format!("get_field({field}, {})", literal(segment));
        }
        ensure!(
            operators(&target.label).contains(&self.operator),
            "{} is not supported for {} fields.",
            self.operator.label(),
            target.label
        );
        let element = target.list_element();
        let kind = FieldType::parse(&element.unwrap_or(target).label);
        let expression = match self.operator {
            FilterOperator::IsNull => format!("{field} IS NULL"),
            FilterOperator::IsNotNull => format!("{field} IS NOT NULL"),
            FilterOperator::IsEmpty if element.is_some() => format!("array_length({field}) = 0"),
            FilterOperator::IsNotEmpty if element.is_some() => format!("array_length({field}) > 0"),
            FilterOperator::IsEmpty => format!("{field} = ''"),
            FilterOperator::IsNotEmpty => format!("{field} <> ''"),
            operator => {
                let value = kind.literal(&self.value)?;
                match operator {
                    FilterOperator::Contains | FilterOperator::NotContains => {
                        let call = if element.is_some() {
                            format!("array_has({field}, {value})")
                        } else {
                            format!("strpos(lower({field}), lower({value})) > 0")
                        };
                        if operator == FilterOperator::NotContains {
                            format!("NOT {call}")
                        } else {
                            call
                        }
                    }
                    FilterOperator::StartsWith => {
                        format!("starts_with(lower({field}), lower({value}))")
                    }
                    FilterOperator::EndsWith => {
                        format!("ends_with(lower({field}), lower({value}))")
                    }
                    FilterOperator::Between => {
                        let upper = kind
                            .literal(&self.second_value)
                            .context("Invalid upper bound.")?;
                        format!("{field} BETWEEN {value} AND {upper}")
                    }
                    operator => {
                        let comparison = match operator {
                            FilterOperator::Eq => "=",
                            FilterOperator::NotEq => "<>",
                            FilterOperator::Gt => ">",
                            FilterOperator::Ge => ">=",
                            FilterOperator::Lt => "<",
                            FilterOperator::Le => "<=",
                            _ => unreachable!("Value-free operators were handled above"),
                        };
                        if matches!(kind, FieldType::Text) {
                            format!("lower({field}) {comparison} lower({value})")
                        } else {
                            format!("{field} {comparison} {value}")
                        }
                    }
                }
            }
        };
        Ok(format!("({expression})"))
    }
}

#[derive(Clone, Copy)]
enum NumberType {
    Signed(u8),
    Unsigned(u8),
    Decimal { precision: usize, scale: usize },
    Float,
    Double,
    BigNum,
}

#[derive(Clone, Copy)]
enum TemporalType {
    Date,
    Time { fraction_digits: usize },
    Timestamp { fraction_digits: usize },
    ZonedTime { fraction_digits: usize },
    ZonedTimestamp { fraction_digits: usize },
}

#[derive(Clone, Copy)]
enum FieldType {
    Text,
    Boolean,
    Number(NumberType),
    Temporal(TemporalType),
    NullOnly,
}

impl FieldType {
    fn parse(data_type: &str) -> Self {
        let upper = data_type
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_uppercase();
        match upper.as_str() {
            "VARCHAR" | "TEXT" | "STRING" | "CHAR" | "BPCHAR" => Self::Text,
            "BOOLEAN" | "BOOL" => Self::Boolean,
            "TINYINT" | "INT1" => Self::Number(NumberType::Signed(8)),
            "SMALLINT" | "INT2" | "SHORT" | "INT16" => Self::Number(NumberType::Signed(16)),
            "INTEGER" | "INT" | "INT4" | "SIGNED" | "INT32" => Self::Number(NumberType::Signed(32)),
            "BIGINT" | "INT8" | "LONG" | "INT64" => Self::Number(NumberType::Signed(64)),
            "HUGEINT" | "INT128" => Self::Number(NumberType::Signed(128)),
            "UTINYINT" | "UINT8" => Self::Number(NumberType::Unsigned(8)),
            "USMALLINT" | "UINT16" => Self::Number(NumberType::Unsigned(16)),
            "UINTEGER" | "UINT32" => Self::Number(NumberType::Unsigned(32)),
            "UBIGINT" | "UINT64" => Self::Number(NumberType::Unsigned(64)),
            "UHUGEINT" | "UINT128" => Self::Number(NumberType::Unsigned(128)),
            "FLOAT" | "REAL" | "FLOAT4" => Self::Number(NumberType::Float),
            "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" => Self::Number(NumberType::Double),
            "BIGNUM" => Self::Number(NumberType::BigNum),
            "DATE" => Self::Temporal(TemporalType::Date),
            "TIME" | "TIME WITHOUT TIME ZONE" => {
                Self::Temporal(TemporalType::Time { fraction_digits: 6 })
            }
            "TIME_NS" => Self::Temporal(TemporalType::Time { fraction_digits: 9 }),
            "TIMETZ" | "TIME WITH TIME ZONE" => {
                Self::Temporal(TemporalType::ZonedTime { fraction_digits: 6 })
            }
            "TIMESTAMP" | "DATETIME" | "TIMESTAMP_US" | "TIMESTAMP WITHOUT TIME ZONE" => {
                Self::Temporal(TemporalType::Timestamp { fraction_digits: 6 })
            }
            "TIMESTAMP_S" => Self::Temporal(TemporalType::Timestamp { fraction_digits: 0 }),
            "TIMESTAMP_MS" => Self::Temporal(TemporalType::Timestamp { fraction_digits: 3 }),
            "TIMESTAMP_NS" => Self::Temporal(TemporalType::Timestamp { fraction_digits: 9 }),
            "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => {
                Self::Temporal(TemporalType::ZonedTimestamp { fraction_digits: 6 })
            }
            _ => {
                if let Some((precision, scale)) = decimal_parameters(&upper) {
                    Self::Number(NumberType::Decimal { precision, scale })
                } else if let Some(kind) = temporal_with_precision(&upper) {
                    Self::Temporal(kind)
                } else if text_with_length(&upper) {
                    Self::Text
                } else {
                    Self::NullOnly
                }
            }
        }
    }

    fn literal(self, value: &str) -> Result<String> {
        ensure!(!value.contains('\0'), "The value cannot contain NUL.");
        match self {
            Self::Text => Ok(literal(value)),
            Self::Boolean => {
                let normalized = match value.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" => "true",
                    "false" | "0" => "false",
                    _ => bail!("Enter true or false for a Boolean value."),
                };
                Ok(format!("CAST('{normalized}' AS BOOLEAN)"))
            }
            Self::Number(kind) => kind.literal(value.trim()),
            Self::Temporal(kind) => kind.literal(value.trim()),
            Self::NullOnly => bail!("Only NULL checks are supported for this field type."),
        }
    }
}

fn temporal_with_precision(data_type: &str) -> Option<TemporalType> {
    let parameter = |prefix: &str| {
        let rest = data_type.strip_prefix(prefix)?.trim().strip_prefix('(')?;
        let (digits, suffix) = rest.split_once(')')?;
        Some((digits.trim().parse::<usize>().ok()?, suffix.trim()))
    };
    if let Some((digits, suffix)) = parameter("TIMESTAMP").or_else(|| parameter("DATETIME")) {
        match suffix {
            "" | "WITHOUT TIME ZONE" if digits <= 9 => {
                return Some(TemporalType::Timestamp {
                    fraction_digits: digits,
                });
            }
            "WITH TIME ZONE" if digits <= 9 => {
                return Some(TemporalType::ZonedTimestamp {
                    fraction_digits: digits,
                });
            }
            _ => return None,
        }
    }
    if let Some((digits, suffix)) = parameter("TIMESTAMPTZ") {
        return (digits <= 9 && suffix.is_empty()).then_some(TemporalType::ZonedTimestamp {
            fraction_digits: digits,
        });
    }
    if let Some((digits, suffix)) = parameter("TIME") {
        match suffix {
            "" | "WITHOUT TIME ZONE" if digits <= 9 => {
                return Some(TemporalType::Time {
                    fraction_digits: digits,
                });
            }
            "WITH TIME ZONE" if digits <= 6 => {
                return Some(TemporalType::ZonedTime {
                    fraction_digits: digits,
                });
            }
            _ => return None,
        }
    }
    if let Some((digits, suffix)) = parameter("TIMETZ") {
        return (digits <= 6 && suffix.is_empty()).then_some(TemporalType::ZonedTime {
            fraction_digits: digits,
        });
    }
    None
}

impl NumberType {
    fn literal(self, value: &str) -> Result<String> {
        let sql_type = match self {
            Self::Signed(bits) => {
                let parsed: i128 = value.parse().context("Enter a whole number in range.")?;
                let fits = bits == 128
                    || (-(1_i128 << (bits - 1))..(1_i128 << (bits - 1))).contains(&parsed);
                ensure!(
                    fits,
                    "The whole number is outside the {bits}-bit signed range."
                );
                match bits {
                    8 => "TINYINT",
                    16 => "SMALLINT",
                    32 => "INTEGER",
                    64 => "BIGINT",
                    _ => "HUGEINT",
                }
                .to_owned()
            }
            Self::Unsigned(bits) => {
                let parsed: u128 = value
                    .parse()
                    .context("Enter a nonnegative whole number in range.")?;
                ensure!(
                    bits == 128 || parsed < (1_u128 << bits),
                    "The whole number is outside the {bits}-bit unsigned range."
                );
                match bits {
                    8 => "UTINYINT",
                    16 => "USMALLINT",
                    32 => "UINTEGER",
                    64 => "UBIGINT",
                    _ => "UHUGEINT",
                }
                .to_owned()
            }
            Self::Decimal { precision, scale } => {
                validate_decimal(value, precision, scale)?;
                format!("DECIMAL({precision},{scale})")
            }
            Self::Float => {
                ensure!(
                    decimal_number_syntax(value) && value.parse::<f32>().is_ok_and(f32::is_finite),
                    "Enter a finite number within the FLOAT range."
                );
                "FLOAT".to_owned()
            }
            Self::Double => {
                ensure!(
                    decimal_number_syntax(value) && value.parse::<f64>().is_ok_and(f64::is_finite),
                    "Enter a finite number within the DOUBLE range."
                );
                "DOUBLE".to_owned()
            }
            Self::BigNum => {
                let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
                ensure!(
                    !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_digit()),
                    "Enter a whole number using digits only."
                );
                "BIGNUM".to_owned()
            }
        };
        // Validation never rewrites the digits through a floating-point value.
        Ok(typed_literal(value, &sql_type))
    }
}

fn decimal_parameters(data_type: &str) -> Option<(usize, usize)> {
    let rest = data_type
        .strip_prefix("DECIMAL")
        .or_else(|| data_type.strip_prefix("NUMERIC"))?
        .trim();
    if rest.is_empty() {
        return Some((18, 3));
    }
    let arguments = rest.strip_prefix('(')?.strip_suffix(')')?;
    let (precision, scale) = arguments.split_once(',').unwrap_or((arguments, "0"));
    let precision: usize = precision.trim().parse().ok()?;
    let scale: usize = scale.trim().parse().ok()?;
    (precision > 0 && precision <= 76 && scale <= precision).then_some((precision, scale))
}

fn text_with_length(data_type: &str) -> bool {
    ["VARCHAR", "CHAR", "BPCHAR"].into_iter().any(|prefix| {
        data_type
            .strip_prefix(prefix)
            .map(str::trim)
            .and_then(|value| value.strip_prefix('('))
            .and_then(|value| value.strip_suffix(')'))
            .is_some_and(|value| {
                let value = value.trim();
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            })
    })
}

fn validate_decimal(value: &str, precision: usize, scale: usize) -> Result<()> {
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    ensure!(
        !(whole.is_empty() && fraction.is_empty())
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.bytes().all(|byte| byte.is_ascii_digit()),
        "Enter a decimal number using digits and a dot (no exponent)."
    );
    ensure!(
        whole.trim_start_matches('0').len() <= precision - scale,
        "The value has too many integer digits for DECIMAL({precision},{scale})."
    );
    ensure!(
        fraction.trim_end_matches('0').len() <= scale,
        "The value has too many fractional digits for DECIMAL({precision},{scale}); rounding is not applied."
    );
    Ok(())
}

fn decimal_number_syntax(value: &str) -> bool {
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map_or((unsigned, None), |(mantissa, exponent)| {
            (mantissa, Some(exponent))
        });
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    !(whole.is_empty() && fraction.is_empty())
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
        && exponent.is_none_or(|exponent| {
            let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
            !exponent.is_empty() && exponent.bytes().all(|byte| byte.is_ascii_digit())
        })
}

impl TemporalType {
    fn literal(self, value: &str) -> Result<String> {
        let sql_type = match self {
            Self::Date => {
                NaiveDate::parse_from_str(value, "%Y-%m-%d")
                    .context("Enter a valid ISO date, such as 2025-01-31.")?;
                "DATE"
            }
            Self::Time {
                fraction_digits: digits,
            } => {
                parse_time(value).context("Enter a valid time, such as 14:30:00.")?;
                ensure!(
                    fraction_digits(value) <= digits,
                    "TIME supports at most {digits} fractional digits."
                );
                if digits > 6 { "TIME_NS" } else { "TIME" }
            }
            Self::Timestamp {
                fraction_digits: digits,
            } => {
                parse_timestamp(value).context(
                    "Enter an ISO date/time without a time zone, such as 2025-01-31 14:30:00.",
                )?;
                ensure!(
                    fraction_digits(value) <= digits,
                    "This timestamp type supports at most {digits} fractional digits."
                );
                match digits {
                    0 => "TIMESTAMP_S",
                    3 => "TIMESTAMP_MS",
                    7..=9 => "TIMESTAMP_NS",
                    _ => "TIMESTAMP",
                }
            }
            Self::ZonedTimestamp {
                fraction_digits: digits,
            } => {
                parse_zoned_timestamp(value).context(
                    "Enter an ISO date/time with an explicit UTC offset, such as 2025-01-31T14:30:00+00:00.",
                )?;
                ensure!(
                    fraction_digits(value) <= digits,
                    "TIMESTAMPTZ supports at most {digits} fractional digits."
                );
                if digits > 6 {
                    "TIMESTAMPTZ_NS"
                } else {
                    "TIMESTAMP WITH TIME ZONE"
                }
            }
            Self::ZonedTime {
                fraction_digits: digits,
            } => {
                parse_zoned_timestamp(&format!("2000-01-01T{value}"))
                    .context("Enter a time with an explicit UTC offset, such as 14:30:00+00:00.")?;
                ensure!(
                    fraction_digits(value) <= digits,
                    "TIMETZ supports at most {digits} fractional digits."
                );
                "TIME WITH TIME ZONE"
            }
        };
        Ok(typed_literal(value, sql_type))
    }
}

fn parse_time(value: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(value, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M"))
        .context("Invalid time")
}

fn parse_timestamp(value: &str) -> Result<NaiveDateTime> {
    ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .into_iter()
        .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
        })
        .context("Invalid date/time")
}

fn parse_zoned_timestamp(value: &str) -> Result<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .or_else(|| {
            [
                "%Y-%m-%d %H:%M:%S%.f%#z",
                "%Y-%m-%dT%H:%M:%S%.f%#z",
                "%Y-%m-%d %H:%M:%S%.f %#z",
            ]
            .into_iter()
            .find_map(|format| DateTime::parse_from_str(value, format).ok())
        })
        .context("Invalid date/time or UTC offset")
}

fn fraction_digits(value: &str) -> usize {
    value.split_once('.').map_or(0, |(_, fraction)| {
        fraction
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .enumerate()
            .fold(
                0,
                |digits, (index, byte)| {
                    if byte == b'0' { digits } else { index + 1 }
                },
            )
    })
}

/// Arrow's own type parser avoids dialect-specific integer aliases and SQL's
/// default microsecond Timestamp coercion. Validation above preserves digits.
fn typed_literal(value: &str, sql_type: &str) -> String {
    let kind = match sql_type {
        "TINYINT" => "Int8".into(),
        "SMALLINT" => "Int16".into(),
        "INTEGER" => "Int32".into(),
        "BIGINT" => "Int64".into(),
        "UTINYINT" => "UInt8".into(),
        "USMALLINT" => "UInt16".into(),
        "UINTEGER" => "UInt32".into(),
        "UBIGINT" => "UInt64".into(),
        "HUGEINT" | "UHUGEINT" => "Decimal256(39, 0)".into(),
        "BIGNUM" => "Decimal256(76, 0)".into(),
        "FLOAT" => "Float32".into(),
        "DOUBLE" => "Float64".into(),
        "DATE" => "Date32".into(),
        "TIME" | "TIME WITH TIME ZONE" => "Time64(Microsecond)".into(),
        "TIME_NS" => "Time64(Nanosecond)".into(),
        "TIMESTAMP_S" => "Timestamp(Second, None)".into(),
        "TIMESTAMP_MS" => "Timestamp(Millisecond, None)".into(),
        "TIMESTAMP_NS" => "Timestamp(Nanosecond, None)".into(),
        "TIMESTAMP" => "Timestamp(Microsecond, None)".into(),
        "TIMESTAMP WITH TIME ZONE" => "Timestamp(Microsecond, Some(\"UTC\"))".into(),
        "TIMESTAMPTZ_NS" => "Timestamp(Nanosecond, Some(\"UTC\"))".into(),
        decimal if decimal.starts_with("DECIMAL(") => {
            let (precision, scale) = decimal_parameters(decimal).expect("validated Decimal");
            format!(
                "{}({precision}, {scale})",
                if precision <= 38 {
                    "Decimal128"
                } else {
                    "Decimal256"
                }
            )
        }
        _ => return format!("CAST({} AS {sql_type})", literal(value)),
    };
    format!("ARROW_CAST({}, {})", literal(value), literal(&kind))
}

fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, ExportFormat, QueryRequest};
    use arrow::array::{Array, BooleanArray};
    use serde_json::json;
    use std::{fs, path::Path};

    const TEXT_FIELD: &str = "客户 \"名]; --";
    const ATTACK_LITERAL: &str = "x'); DROP TABLE data; -- FROM '/tmp/source'";

    fn column(name: &str, data_type: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    fn condition(field: &str, operator: FilterOperator, value: &str) -> FilterCondition {
        FilterCondition {
            field: field.into(),
            path: Vec::new(),
            operator,
            value: value.into(),
            second_value: String::new(),
        }
    }

    fn group(conditions: Vec<FilterCondition>) -> FilterGroup {
        FilterGroup {
            mode: MatchMode::All,
            conditions,
        }
    }

    fn one(field: &str, operator: FilterOperator, value: &str) -> FilterGroup {
        group(vec![condition(field, operator, value)])
    }

    fn between(field: &str, lower: &str, upper: &str) -> FilterGroup {
        let mut condition = condition(field, FilterOperator::Between, lower);
        condition.second_value = upper.into();
        group(vec![condition])
    }

    fn child(field: &str, path: &[&str], operator: FilterOperator, value: &str) -> FilterGroup {
        let mut condition = condition(field, operator, value);
        condition.path = path.iter().map(|segment| (*segment).to_owned()).collect();
        group(vec![condition])
    }

    fn ids(engine: &mut Engine, filter: &FilterGroup) -> Result<Vec<i32>> {
        let filter = filter.to_filter(&engine.info.columns)?;
        let result = engine.query(&QueryRequest {
            filter,
            sort: Some(("id".into(), false)),
            ..Default::default()
        })?;
        (0..result.row_count())
            .map(|row| {
                result
                    .cell(row, 0)
                    .text
                    .parse()
                    .context("Expected integer id")
            })
            .collect()
    }

    fn parquet_fixture(path: &Path) -> Result<()> {
        let columns = vec![
            column("id", "INTEGER"),
            column(TEXT_FIELD, "VARCHAR"),
            column("big", "BIGINT"),
            column("amount", "DECIMAL(38,10)"),
            column("day", "DATE"),
            column("stamp", "TIMESTAMP"),
            column("flag", "BOOLEAN"),
            column("nested", "STRUCT(inner_value VARCHAR)"),
            column("binary", "BLOB"),
        ];
        let mut rows = vec![
            json!({"id":0,"big":"9007199254740993","amount":"12345678901234567890.1234567890","day":"2025-01-01","stamp":"2025-01-01 12:30:00.123456","flag":true,"nested":{"inner_value":"中文"},"binary":[0,255]}),
            json!({"id":1,"big":"9007199254740994","amount":"12345678901234567890.1234567891","day":"2025-01-02","stamp":"2025-01-02 12:30:00","flag":false}),
            json!({"id":2}),
            json!({"id":3,"big":-1,"amount":"-1.0000000000","day":"2025-01-04","stamp":"2025-01-04","flag":true}),
            json!({"id":4,"big":"9223372036854775807","amount":"0","day":"2025-01-05","stamp":"2025-01-05","flag":true}),
            json!({"id":5,"big":0,"amount":"0.0000000001","day":"2025-01-06","stamp":"2025-01-06","flag":false}),
        ];
        for (row, text) in rows.iter_mut().zip([
            Some("Alpha_%中文O'Brien"),
            Some("beta"),
            None,
            Some(""),
            Some("ALPHA finish"),
            Some(ATTACK_LITERAL),
        ]) {
            row[TEXT_FIELD] = text.map_or(serde_json::Value::Null, |value| json!(value));
        }
        crate::test_support::write_rows(path, &columns, &rows)
    }

    fn evaluate(sql: &str) -> Result<bool> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let batches = runtime.block_on(async {
            datafusion::prelude::SessionContext::new()
                .sql(sql)
                .await?
                .collect()
                .await
        })?;
        let values = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .context("Expected Boolean expression")?;
        Ok(!values.is_null(0) && values.value(0))
    }

    #[test]
    fn operator_choices_are_schema_bound_and_unknown_types_are_null_only() {
        for data_type in ["VARCHAR", "CHAR(20)", "TEXT"] {
            assert_eq!(operators(data_type), TEXT_OPERATORS);
        }
        for data_type in [
            "BIGINT",
            "UHUGEINT",
            "DECIMAL(38, 10)",
            "DECIMAL(39,0)",
            "DECIMAL(76,20)",
            "DOUBLE",
            "DATE",
            "TIMESTAMP_NS",
            "TIMESTAMP WITH TIME ZONE",
        ] {
            assert_eq!(operators(data_type), ORDERED_OPERATORS, "{data_type}");
        }
        assert_eq!(operators("BOOLEAN"), BOOLEAN_OPERATORS);
        for data_type in [
            "STRUCT(a INTEGER)",
            "MAP(VARCHAR,INTEGER)",
            "BLOB",
            "UUID",
            "JSON",
            "DECIMAL(77,0)",
            "INTEGER); DROP TABLE data; --",
        ] {
            assert_eq!(operators(data_type), NULL_OPERATORS, "{data_type}");
        }
        assert_eq!(operators("INTEGER[]"), LIST_OPERATORS);
        assert_eq!(operators("STRUCT(a INTEGER)[]"), NESTED_LIST_OPERATORS);
        assert!(FilterOperator::Between.needs_value());
        assert!(!FilterOperator::IsEmpty.needs_value());
        assert!(!FilterOperator::IsNull.needs_value());
        assert_eq!(FilterGroup::default().mode, MatchMode::All);
        assert_eq!(FilterGroup::default().to_filter(&[]).unwrap(), "");
    }

    #[test]
    fn editor_type_classification_and_validation_use_the_same_normalized_schema() -> Result<()> {
        for (data_type, expected) in [
            (" int64 ", FilterFieldKind::Number),
            ("UInt64", FilterFieldKind::Number),
            (" double   precision ", FilterFieldKind::Number),
            ("numeric ( 38 , 10 )", FilterFieldKind::Number),
            ("NUMERIC(18)", FilterFieldKind::Number),
            ("DECIMAL", FilterFieldKind::Number),
            ("BIGNUM", FilterFieldKind::Number),
            (" varchar ( 20 ) ", FilterFieldKind::Text),
            (" bool ", FilterFieldKind::Boolean),
            (" date ", FilterFieldKind::Date),
            (
                "timestamp ( 3 ) without   time zone",
                FilterFieldKind::DateTime,
            ),
            ("TIMESTAMP(6) WITH TIME ZONE", FilterFieldKind::DateTime),
            ("timestamp_ns", FilterFieldKind::DateTime),
            ("time ( 9 )", FilterFieldKind::Time),
            ("time_ns", FilterFieldKind::Time),
            (" time with   time zone ", FilterFieldKind::Time),
            ("TIMETZ(3)", FilterFieldKind::Time),
            ("INTEGER[]", FilterFieldKind::List),
            ("DECIMAL(20,4)[3]", FilterFieldKind::List),
            ("TIMESTAMP(6)[]", FilterFieldKind::List),
            ("TIMESTAMP(10)", FilterFieldKind::NullOnly),
            ("TIME(9) WITH TIME ZONE", FilterFieldKind::NullOnly),
            ("STRUCT(value INTEGER)", FilterFieldKind::NullOnly),
            ("BLOB", FilterFieldKind::NullOnly),
            ("UUID", FilterFieldKind::NullOnly),
        ] {
            assert_eq!(field_kind(data_type), expected, "{data_type}");
            assert_eq!(
                operators(data_type),
                match expected {
                    FilterFieldKind::Number
                    | FilterFieldKind::Date
                    | FilterFieldKind::DateTime
                    | FilterFieldKind::Time => ORDERED_OPERATORS,
                    FilterFieldKind::Boolean => BOOLEAN_OPERATORS,
                    FilterFieldKind::Text => TEXT_OPERATORS,
                    FilterFieldKind::List => LIST_OPERATORS,
                    FilterFieldKind::NullOnly => NULL_OPERATORS,
                },
                "{data_type}"
            );
        }
        // Numeric-looking text keeps literal text behavior. Changing the same
        // field's schema cannot leave a stale text operator valid for a number.
        assert!(
            one("value", FilterOperator::Eq, "00123")
                .to_filter(&[column("value", "VARCHAR")])?
                .contains("lower('00123')")
        );
        assert!(
            one("value", FilterOperator::Contains, "12")
                .to_filter(&[column("value", "INTEGER")])
                .is_err()
        );
        assert!(
            between("value", "0", "1")
                .to_filter(&[column("value", "BOOLEAN")])
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn temporal_operator_labels_and_input_patterns_match_the_actual_comparison() {
        for (data_type, equal, between_label, pattern) in [
            ("DATE", "On date", "Between dates (inclusive)", "YYYY-MM-DD"),
            (
                "TIMESTAMP_NS",
                "At date/time",
                "Between date/times (inclusive)",
                "YYYY-MM-DD HH:MM:SS",
            ),
            (
                "TIMESTAMP(6) WITH TIME ZONE",
                "At date/time",
                "Between date/times (inclusive)",
                "YYYY-MM-DD HH:MM:SS+HH:MM",
            ),
            ("TIME", "At time", "Between times (inclusive)", "HH:MM:SS"),
            (
                "TIME WITH TIME ZONE",
                "At time",
                "Between times (inclusive)",
                "HH:MM:SS+HH:MM",
            ),
        ] {
            assert_eq!(operator_label(FilterOperator::Eq, data_type), equal);
            assert_eq!(operator_label(FilterOperator::Gt, data_type), "After");
            assert_eq!(operator_label(FilterOperator::Lt, data_type), "Before");
            assert_eq!(
                operator_label(FilterOperator::Between, data_type),
                between_label
            );
            assert_eq!(value_placeholder(data_type, false), pattern);
            assert_eq!(value_placeholder(data_type, true), pattern);
        }
        assert_eq!(operator_label(FilterOperator::Ge, "DATE"), "On or after");
        assert_eq!(
            operator_label(FilterOperator::Ge, "TIMESTAMP"),
            "At or after"
        );
        assert_eq!(operator_label(FilterOperator::Gt, "BIGINT"), "Greater than");
        assert_eq!(value_placeholder("BIGINT", false), "Number");
        assert_eq!(value_placeholder("BIGINT", true), "Upper bound");
        assert_eq!(value_placeholder("BOOL", false), "true / false");
    }

    #[test]
    fn struct_targets_keep_exact_quoted_paths_and_list_element_types() -> Result<()> {
        let columns = [
            column(
                "profile",
                "STRUCT(age INTEGER, \"a.b\" VARCHAR, \"inner\" STRUCT(flag BOOLEAN, dates DATE[]), records STRUCT(code INTEGER)[])",
            ),
            column("profile.age", "VARCHAR"),
            column("numbers", "BIGINT[3]"),
        ];
        let targets = targets(&columns);
        let names = targets
            .iter()
            .map(|target| target.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "profile",
                "\"profile\".\"age\"",
                "\"profile\".\"a.b\"",
                "\"profile\".\"inner\"",
                "\"profile\".\"inner\".\"flag\"",
                "\"profile\".\"inner\".\"dates\"",
                "\"profile\".\"records\"",
                "profile.age",
                "numbers",
            ]
        );
        let dates = &targets[5];
        assert!(dates.is_list);
        assert_eq!(dates.path, ["inner", "dates"]);
        assert_eq!(dates.data_type, "DATE[]");
        assert_eq!(dates.value_type, "DATE");
        assert_eq!(value_kind(&dates.data_type), FilterFieldKind::Date);
        assert_eq!(value_placeholder(&dates.data_type, false), "YYYY-MM-DD");
        assert_eq!(list_element_type("BIGINT[3]").as_deref(), Some("BIGINT"));
        assert_eq!(operators(&targets[6].data_type), NESTED_LIST_OPERATORS);
        assert!(
            !targets
                .iter()
                .any(|target| target.path == ["records", "code"])
        );
        assert_eq!(
            child("profile", &["a.b"], FilterOperator::Eq, "x").to_filter(&columns)?,
            "((lower(get_field(\"profile\", 'a.b')) = lower('x')))"
        );
        for path in [
            &["a", "b"][..],
            &["inner", "missing"][..],
            &["records", "code"][..],
            &["AGE"][..],
        ] {
            assert!(
                child("profile", path, FilterOperator::IsNull, "")
                    .to_filter(&columns)
                    .is_err(),
                "{path:?}"
            );
        }
        assert!(
            child("numbers", &["code"], FilterOperator::IsNull, "")
                .to_filter(&columns)
                .is_err()
        );
        assert!(
            child("profile", &["\0"], FilterOperator::IsNull, "")
                .to_filter(&columns)
                .is_err()
        );
        assert!(
            child("profile", &["v"], FilterOperator::Eq, "1")
                .to_filter(&[column("profile", "STRUCT(v INTEGER, V INTEGER)")])
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn parquet_struct_paths_and_primitive_lists_preserve_exact_values_and_null_semantics()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nested_filters.parquet");
        let columns = vec![
            column("id", "INTEGER"),
            column(
                "profile",
                "STRUCT(age INTEGER, name VARCHAR, \"inner\" STRUCT(active BOOLEAN, \"a.b\" DECIMAL(38,18)), tags VARCHAR[])",
            ),
            column("numbers", "BIGINT[]"),
            column("flags", "BOOLEAN[]"),
            column("dates", "DATE[]"),
            column("amounts", "DECIMAL(38,18)[]"),
            column("records", "STRUCT(code INTEGER)[]"),
            column("grid", "INTEGER[][]"),
        ];
        let rows = vec![
            json!({"id":0,"profile":{"age":10,"name":"Alpha","inner":{"active":true,"a.b":"12345678901234567890.123456789012345678"},"tags":["Alpha",null,"O'Brien"]},"numbers":["9007199254740993",null,2],"flags":[true,null],"dates":["2024-02-29",null],"amounts":["12345678901234567890.123456789012345678",null],"records":[{"code":1},null],"grid":[[1,null],[],null]}),
            json!({"id":1,"profile":{"age":20,"name":"Beta","inner":{"active":false,"a.b":"12345678901234567890.123456789012345679"},"tags":["beta"]},"numbers":["9007199254740994"],"flags":[false],"dates":["2024-03-01"],"amounts":["12345678901234567890.123456789012345679"],"records":[],"grid":[]}),
            json!({"id":2}),
            json!({"id":3,"profile":{"age":null,"name":null,"inner":null,"tags":[]},"numbers":[],"flags":[],"dates":[],"amounts":[],"records":[null],"grid":[null]}),
            json!({"id":4,"profile":{"age":30,"name":"","inner":{"active":true,"a.b":"0"},"tags":null},"numbers":[null],"flags":[null],"dates":[null],"amounts":[null],"records":[{"code":2}],"grid":[[]]}),
        ];
        crate::test_support::write_rows(&path, &columns, &rows)?;
        let before = fs::read(&path)?;
        let mut engine = Engine::open(&[path.clone()])?;
        let cases = [
            (
                child("profile", &["age"], FilterOperator::Gt, "15"),
                vec![1, 4],
            ),
            (
                child("profile", &["name"], FilterOperator::Contains, "ALPHA"),
                vec![0],
            ),
            (
                child("profile", &["inner", "active"], FilterOperator::Eq, "false"),
                vec![1],
            ),
            (
                child(
                    "profile",
                    &["inner", "a.b"],
                    FilterOperator::Eq,
                    "12345678901234567890.123456789012345679",
                ),
                vec![1],
            ),
            (
                child("profile", &["inner", "active"], FilterOperator::IsNull, ""),
                vec![2, 3],
            ),
            (one("profile", FilterOperator::IsNull, ""), vec![2]),
            (
                child("profile", &["tags"], FilterOperator::Contains, "Alpha"),
                vec![0],
            ),
            (
                child("profile", &["tags"], FilterOperator::Contains, "alpha"),
                vec![],
            ),
            (
                child("profile", &["tags"], FilterOperator::Contains, "Al"),
                vec![],
            ),
            (
                child("profile", &["tags"], FilterOperator::Contains, "O'Brien"),
                vec![0],
            ),
            (
                child("profile", &["tags"], FilterOperator::NotContains, "Alpha"),
                vec![1, 3],
            ),
            (
                one("numbers", FilterOperator::Contains, "9007199254740993"),
                vec![0],
            ),
            (
                one("numbers", FilterOperator::Contains, "9007199254740994"),
                vec![1],
            ),
            (
                one("numbers", FilterOperator::NotContains, "9007199254740993"),
                vec![1, 3, 4],
            ),
            (one("numbers", FilterOperator::IsEmpty, ""), vec![3]),
            (
                one("numbers", FilterOperator::IsNotEmpty, ""),
                vec![0, 1, 4],
            ),
            (one("numbers", FilterOperator::IsNull, ""), vec![2]),
            (one("flags", FilterOperator::Contains, "true"), vec![0]),
            (
                one("flags", FilterOperator::NotContains, "true"),
                vec![1, 3, 4],
            ),
            (
                one("dates", FilterOperator::Contains, "2024-02-29"),
                vec![0],
            ),
            (
                one(
                    "amounts",
                    FilterOperator::Contains,
                    "12345678901234567890.123456789012345679",
                ),
                vec![1],
            ),
            (one("records", FilterOperator::IsEmpty, ""), vec![1]),
            (
                one("records", FilterOperator::IsNotEmpty, ""),
                vec![0, 3, 4],
            ),
            (one("records", FilterOperator::IsNull, ""), vec![2]),
            (one("grid", FilterOperator::IsEmpty, ""), vec![1]),
        ];
        for (filter, expected) in cases {
            assert_eq!(ids(&mut engine, &filter)?, expected, "{filter:?}");
        }
        for filter in [
            one("numbers", FilterOperator::Contains, "NULL"),
            one("dates", FilterOperator::Contains, "2025-02-29"),
            one("amounts", FilterOperator::Contains, "0.1234567890123456789"),
            one("records", FilterOperator::Contains, "{}"),
            one("grid", FilterOperator::Contains, "[1]"),
            one("numbers", FilterOperator::Eq, "1"),
            one("numbers", FilterOperator::StartsWith, "1"),
        ] {
            assert!(
                filter.to_filter(&engine.info.columns).is_err(),
                "{filter:?}"
            );
        }
        assert_eq!(fs::read(path)?, before);
        Ok(())
    }

    #[test]
    fn quoted_struct_names_and_values_remain_literals_through_engine_validation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("quoted_nested.parquet");
        let root = "profile.dot\"' ; --";
        let leaf = "child.dot\"' FROM ; --";
        let value = "x'); DROP TABLE data; -- FROM '/tmp/source'";
        let columns = vec![
            column("id", "INTEGER"),
            column(root, &format!("STRUCT({} VARCHAR)", identifier(leaf))),
            column("profile", "STRUCT(age INTEGER)"),
            column("profile.age", "INTEGER"),
        ];
        let mut rows = vec![
            json!({"id":0,"profile":{"age":10},"profile.age":999}),
            json!({"id":1,"profile":{"age":20},"profile.age":10}),
        ];
        rows[0][root] = json!({leaf:value});
        rows[1][root] = json!({leaf:"safe"});
        crate::test_support::write_rows(&path, &columns, &rows)?;
        let mut engine = Engine::open(&[path.clone()])?;
        assert_eq!(
            ids(
                &mut engine,
                &child("profile", &["age"], FilterOperator::Eq, "10")
            )?,
            [0]
        );
        assert_eq!(
            ids(&mut engine, &one("profile.age", FilterOperator::Eq, "10"))?,
            [1]
        );
        let filter = child(root, &[leaf], FilterOperator::Eq, value);
        assert_eq!(ids(&mut engine, &filter)?, [0]);
        // The same payload in a path is rejected unless it exists in Schema.
        assert!(
            child(
                root,
                &["x'); DROP TABLE data; --"],
                FilterOperator::IsNull,
                ""
            )
            .to_filter(&engine.info.columns)
            .is_err()
        );
        assert_eq!(ids(&mut engine, &filter)?, [0]);
        Ok(())
    }

    #[test]
    fn normalized_precision_and_unbounded_integer_filters_execute_without_rounding() -> Result<()> {
        for (data_type, valid, invalid) in [
            ("TIME_NS", "14:30:00.123456789", "14:30:00.1234567891"),
            ("TIME(3)", "14:30:00.123", "14:30:00.1231"),
            (
                "TIMESTAMP(7)",
                "2025-01-31 14:30:00.1234567",
                "2025-01-31 14:30:00.12345671",
            ),
            (
                "timestamp (3) without time zone",
                "2025-01-31 14:30:00.123",
                "2025-01-31 14:30:00.1231",
            ),
            (
                "TIMESTAMPTZ(3)",
                "2025-01-31T14:30:00.123+08:00",
                "2025-01-31T14:30:00.1231+08:00",
            ),
            ("NUMERIC(5)", "12345", "12345.1"),
            (
                "BIGNUM",
                "12345678901234567890123456789012345678901234567890",
                "1e50",
            ),
        ] {
            let schema = [column("value", data_type)];
            let filter = one("value", FilterOperator::Eq, valid).to_filter(&schema)?;
            let exact = FieldType::parse(data_type).literal(valid)?;
            let matches = evaluate(&format!(
                "SELECT {filter} FROM (SELECT {exact} AS value) typed_input"
            ))?;
            assert!(matches, "{data_type}: {filter}");
            assert!(
                one("value", FilterOperator::Eq, invalid)
                    .to_filter(&schema)
                    .is_err(),
                "{data_type}"
            );
        }
        // Temporal equality is never rewritten into equality on just the date.
        let filter = one("value", FilterOperator::Eq, "2025-01-31")
            .to_filter(&[column("value", "TIMESTAMP_NS")])?;
        let midday_matches = evaluate(&format!(
            "SELECT {filter} FROM (SELECT ARROW_CAST('2025-01-31 12:00:00', 'Timestamp(Nanosecond, None)') AS value) typed_input"
        ))?;
        assert!(!midday_matches);
        Ok(())
    }

    #[test]
    fn parquet_decimal256_filters_keep_every_input_digit() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("decimal256.parquet");
        let columns = vec![column("id", "INTEGER"), column("amount", "DECIMAL(60,20)")];
        let first = "12345678901234567890123456789012345.12345678901234567890";
        let second = "12345678901234567890123456789012345.12345678901234567891";
        crate::test_support::write_rows(
            &path,
            &columns,
            &[
                json!({"id":0,"amount":first}),
                json!({"id":1,"amount":second}),
                json!({"id":2}),
            ],
        )?;
        let mut engine = Engine::open(&[path])?;
        assert_eq!(
            ids(&mut engine, &one("amount", FilterOperator::Eq, second))?,
            [1]
        );
        assert_eq!(ids(&mut engine, &between("amount", first, second))?, [0, 1]);
        assert_eq!(
            ids(&mut engine, &one("amount", FilterOperator::IsNull, ""))?,
            [2]
        );
        Ok(())
    }

    #[test]
    fn global_nested_sort_across_row_groups_preserves_original_row_ids() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("nested_sort.parquet");
        let columns = vec![
            column("id", "INTEGER"),
            column("record", "STRUCT(x INTEGER)"),
            column("items", "INTEGER[]"),
            column("mapping", "MAP(VARCHAR,INTEGER)"),
        ];
        let batch = crate::test_support::batch_from_json(
            crate::columnar::fixture_schema(&columns)?,
            &[
                json!({"id":0,"record":{"x":2},"items":[2],"mapping":{"key":2}}),
                json!({"id":1,"record":{"x":0},"items":[0],"mapping":{"key":0}}),
                json!({"id":2,"record":{"x":1},"items":[1],"mapping":{"key":1}}),
                json!({"id":3}),
            ],
        )?;
        let properties = parquet::file::properties::WriterProperties::builder()
            .set_max_row_group_row_count(Some(1))
            .build();
        let mut writer = parquet::arrow::ArrowWriter::try_new(
            fs::File::create_new(&path)?,
            batch.schema(),
            Some(properties),
        )?;
        writer.write(&batch)?;
        writer.close()?;
        let mut engine = Engine::open(&[path])?;
        assert_eq!(engine.info.row_groups, 4);
        for name in ["record", "items", "mapping"] {
            let result = engine.query(&QueryRequest {
                sort: Some((name.into(), false)),
                ..Default::default()
            })?;
            assert_eq!(
                (0..4)
                    .map(|row| result.cell(row, 0).text)
                    .collect::<Vec<_>>(),
                ["1", "2", "0", "3"],
                "{name}"
            );
            assert_eq!(
                (0..4)
                    .map(|row| result.source_row_number(row))
                    .collect::<Vec<_>>(),
                [Some(2), Some(3), Some(1), Some(4)],
                "{name}"
            );
        }
        Ok(())
    }

    #[test]
    fn exact_numbers_and_temporal_values_are_validated_without_rounding() -> Result<()> {
        for (data_type, valid, invalid) in [
            ("TINYINT", "-128", "128"),
            ("UTINYINT", "255", "-1"),
            ("BIGINT", "9223372036854775807", "9223372036854775808"),
            ("INT64", "9223372036854775807", "9223372036854775808"),
            ("UBIGINT", "18446744073709551615", "18446744073709551616"),
            (
                "HUGEINT",
                "170141183460469231731687303715884105727",
                "170141183460469231731687303715884105728",
            ),
            (
                "UHUGEINT",
                "340282366920938463463374607431768211455",
                "340282366920938463463374607431768211456",
            ),
            ("DECIMAL(5,2)", "123.4500", "123.456"),
            ("DECIMAL(2,2)", ".99", "1"),
            ("FLOAT", "1.25e3", "1e100"),
            ("DOUBLE", "-1.25e-20", "NaN"),
            ("DATE", "2024-02-29", "2025-02-29"),
            ("TIME", "14:30:00.123456", "25:00:00"),
            (
                "TIMESTAMP",
                "2025-01-31T14:30:00.123456",
                "2025-02-30 12:00:00",
            ),
            (
                "TIMESTAMP_MS",
                "2025-01-31 14:30:00.123",
                "2025-01-31 14:30:00.1234",
            ),
            (
                "TIMESTAMPTZ",
                "2025-01-31T14:30:00+08:00",
                "2025-01-31 14:30:00",
            ),
            ("BOOLEAN", "TRUE", "maybe"),
        ] {
            let columns = [column("value", data_type)];
            let sql = one("value", FilterOperator::Eq, valid).to_filter(&columns)?;
            assert!(sql.contains("CAST("), "{data_type}: {sql}");
            let error = one("value", FilterOperator::Eq, invalid)
                .to_filter(&columns)
                .unwrap_err();
            let error = format!("{error:#}");
            assert!(
                error.contains("Condition 1") && error.contains("value"),
                "{error}"
            );
        }
        let sql = one("big", FilterOperator::Eq, "9007199254740993")
            .to_filter(&[column("big", "HUGEINT")])?;
        assert!(sql.contains("ARROW_CAST('9007199254740993', 'Decimal256(39, 0)')"));
        let sql = one(
            "amount",
            FilterOperator::Eq,
            "12345678901234567890.1234567890",
        )
        .to_filter(&[column("amount", "DECIMAL(38,10)")])?;
        assert!(
            sql.contains("ARROW_CAST('12345678901234567890.1234567890', 'Decimal128(38, 10)')")
        );
        Ok(())
    }

    #[test]
    fn errors_identify_the_condition_field_and_upper_bound() {
        let columns = [column("id", "INTEGER"), column("nested", "INTEGER[]")];
        for filter in [
            one("missing", FilterOperator::Eq, "1"),
            one("id", FilterOperator::Contains, "1"),
            one("nested", FilterOperator::Eq, "[1]"),
            one("id", FilterOperator::Eq, "1; SELECT 1"),
        ] {
            let error = format!("{:#}", filter.to_filter(&columns).unwrap_err());
            assert!(error.contains("Condition 1"), "{error}");
            assert!(error.contains(&filter.conditions[0].field), "{error}");
        }
        let error = format!(
            "{:#}",
            between("id", "1", "oops").to_filter(&columns).unwrap_err()
        );
        assert!(error.contains("upper bound"), "{error}");
        let error = group(vec![
            condition("id", FilterOperator::Ge, "1"),
            condition("id", FilterOperator::Le, ""),
        ])
        .to_filter(&columns)
        .unwrap_err();
        assert!(format!("{error:#}").contains("Condition 2"));
        assert!(
            one("text", FilterOperator::Eq, "\0")
                .to_filter(&[column("text", "VARCHAR")])
                .is_err()
        );
    }

    #[test]
    fn parquet_and_or_repeated_fields_and_loaded_window_scope() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("visual ' filter.parquet");
        parquet_fixture(&path)?;
        let mut engine = Engine::open(&[path])?;
        let mut filter = group(vec![
            condition("id", FilterOperator::Ge, "1"),
            condition("id", FilterOperator::Le, "3"),
        ]);
        assert_eq!(ids(&mut engine, &filter)?, [1, 2, 3]);
        filter.mode = MatchMode::Any;
        assert_eq!(ids(&mut engine, &filter)?, [0, 1, 2, 3, 4, 5]);
        let filter = group(vec![
            condition("flag", FilterOperator::Eq, "true"),
            condition("id", FilterOperator::Gt, "0"),
        ]);
        assert_eq!(ids(&mut engine, &filter)?, [3, 4]);
        let filter = one("id", FilterOperator::Gt, "2").to_filter(&engine.info.columns)?;
        let result = engine.query(&QueryRequest {
            filter: filter.clone(),
            limit: 3,
            ..Default::default()
        })?;
        assert_eq!(result.loaded_rows, 3);
        assert!(result.is_empty());
        let result = engine.query(&QueryRequest {
            filter,
            offset: 3,
            limit: 2,
            ..Default::default()
        })?;
        assert_eq!(result.row_count(), 2);
        assert_eq!(result.cell(0, 0).text, "3");
        Ok(())
    }

    #[test]
    fn parquet_literal_text_matching_quotes_unicode_null_and_empty() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("text.parquet");
        parquet_fixture(&path)?;
        let mut engine = Engine::open(&[path.clone()])?;
        let before = fs::read(&path)?;
        for (operator, value, expected) in [
            (FilterOperator::Contains, "_%中文o'brien", vec![0]),
            (FilterOperator::NotContains, "ALPHA", vec![1, 3, 5]),
            (FilterOperator::StartsWith, "alpha_%", vec![0]),
            (FilterOperator::EndsWith, "O'BRIEN", vec![0]),
            (FilterOperator::Eq, "BETA", vec![1]),
            (FilterOperator::NotEq, "BETA", vec![0, 3, 4, 5]),
            (FilterOperator::Eq, "", vec![3]),
            (FilterOperator::IsEmpty, "ignored", vec![3]),
            (FilterOperator::IsNotEmpty, "", vec![0, 1, 4, 5]),
            (FilterOperator::IsNull, "", vec![2]),
            (FilterOperator::IsNotNull, "", vec![0, 1, 3, 4, 5]),
            (FilterOperator::Eq, ATTACK_LITERAL, vec![5]),
            (FilterOperator::Contains, "-- FROM '/tmp/source'", vec![5]),
        ] {
            assert_eq!(
                ids(&mut engine, &one(TEXT_FIELD, operator, value))?,
                expected,
                "{} {value:?}",
                operator.label()
            );
        }
        assert_eq!(
            ids(&mut engine, &one("nested", FilterOperator::IsNotNull, ""))?,
            [0]
        );
        assert_eq!(
            ids(&mut engine, &one("binary", FilterOperator::IsNull, ""))?,
            [1, 2, 3, 4, 5]
        );
        assert_eq!(fs::read(path)?, before);
        Ok(())
    }

    #[test]
    fn parquet_numeric_precision_dates_booleans_and_inclusive_ranges() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("typed.parquet");
        parquet_fixture(&path)?;
        let mut engine = Engine::open(&[path])?;
        for (operator, expected) in [
            (FilterOperator::Eq, vec![2]),
            (FilterOperator::NotEq, vec![0, 1, 3, 4, 5]),
            (FilterOperator::Gt, vec![3, 4, 5]),
            (FilterOperator::Ge, vec![2, 3, 4, 5]),
            (FilterOperator::Lt, vec![0, 1]),
            (FilterOperator::Le, vec![0, 1, 2]),
        ] {
            assert_eq!(ids(&mut engine, &one("id", operator, "2"))?, expected);
        }
        for (filter, expected) in [
            (between("id", "2", "4"), vec![2, 3, 4]),
            (one("big", FilterOperator::Eq, "9007199254740993"), vec![0]),
            (
                one("big", FilterOperator::Eq, "9223372036854775807"),
                vec![4],
            ),
            (
                one(
                    "amount",
                    FilterOperator::Eq,
                    "12345678901234567890.1234567890",
                ),
                vec![0],
            ),
            (
                between(
                    "amount",
                    "12345678901234567890.1234567890",
                    "12345678901234567890.1234567891",
                ),
                vec![0, 1],
            ),
            (between("day", "2025-01-01", "2025-01-02"), vec![0, 1]),
            (one("day", FilterOperator::Gt, "2025-01-04"), vec![4, 5]),
            (
                one("stamp", FilterOperator::Eq, "2025-01-01T12:30:00.123456"),
                vec![0],
            ),
            (one("flag", FilterOperator::Eq, "0"), vec![1, 5]),
            (one("flag", FilterOperator::IsNull, ""), vec![2]),
        ] {
            assert_eq!(ids(&mut engine, &filter)?, expected, "{filter:?}");
        }
        Ok(())
    }

    #[test]
    fn rejected_model_and_engine_query_keep_previous_snapshot_exportable() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source.parquet");
        parquet_fixture(&source)?;
        let mut engine = Engine::open(&[source.clone()])?;
        let first = engine.query(&QueryRequest {
            filter: one("id", FilterOperator::Eq, "1").to_filter(&engine.info.columns)?,
            ..Default::default()
        })?;
        assert!(
            one("amount", FilterOperator::Eq, "0.12345678901")
                .to_filter(&engine.info.columns)
                .is_err()
        );
        // A valid model referring to an unloaded field still fails safely in the
        // engine; the UI prevents this case before applying its field selection.
        let filter =
            one("big", FilterOperator::Eq, "9007199254740993").to_filter(&engine.info.columns)?;
        assert!(
            engine
                .query(&QueryRequest {
                    selected_columns: vec!["id".into()],
                    filter,
                    ..Default::default()
                })
                .is_err()
        );
        let destination = dir.path().join("previous.json");
        engine.export_result(&first.snapshot_id, &destination, ExportFormat::Json)?;
        let rows: serde_json::Value = serde_json::from_slice(&fs::read(destination)?)?;
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["id"], 1);
        assert!(
            engine
                .export_result(&first.snapshot_id, &source, ExportFormat::Json)
                .is_err()
        );
        assert_eq!(ids(&mut engine, &one("id", FilterOperator::Eq, "4"))?, [4]);
        Ok(())
    }
}
