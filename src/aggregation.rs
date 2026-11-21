//! Schema-bound grouping and aggregates over immutable imported snapshots.
//! DataFusion performs the value operations; this module validates metadata and
//! creates quoted expressions, never per-row Rust grouping or JSON conversion.
use crate::{columnar::ArrowTable, engine::ColumnInfo, filtering::FilterFieldKind};
use anyhow::{Context, Result, ensure};
use arrow::{
    array::ArrayRef,
    compute::{DatePart, cast, date_part},
    datatypes::{DataType, Field, Schema, TimeUnit},
};
use datafusion::{
    common::ScalarValue,
    functions::datetime::date_trunc,
    logical_expr::{ColumnarValue, ScalarFunctionArgs},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AggregateOp {
    #[default]
    CountRows,
    Count,
    NUnique,
    Sum,
    Mean,
    Min,
    Max,
    MissingRatio,
    StdDevSample,
    StdDevPopulation,
    MedianExact,
    P25Exact,
    P75Exact,
    P90Exact,
    P95Exact,
    MedianApprox,
    P25Approx,
    P75Approx,
    P90Approx,
    P95Approx,
}

impl AggregateOp {
    pub fn label(self) -> &'static str {
        match self {
            Self::CountRows => "Count rows",
            Self::Count => "Count",
            Self::NUnique => "NUnique",
            Self::Sum => "Sum",
            Self::Mean => "Mean",
            Self::Min => "Minimum",
            Self::Max => "Maximum",
            Self::MissingRatio => "Missing ratio",
            Self::StdDevSample => "StdDev (sample)",
            Self::StdDevPopulation => "StdDev (population)",
            Self::MedianExact => "Median (exact)",
            Self::P25Exact => "P25 (exact)",
            Self::P75Exact => "P75 (exact)",
            Self::P90Exact => "P90 (exact)",
            Self::P95Exact => "P95 (exact)",
            Self::MedianApprox => "Median (approx.)",
            Self::P25Approx => "P25 (approx.)",
            Self::P75Approx => "P75 (approx.)",
            Self::P90Approx => "P90 (approx.)",
            Self::P95Approx => "P95 (approx.)",
        }
    }

    fn percentile(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            Self::MedianExact => ("percentile_cont", "0.5"),
            Self::P25Exact => ("percentile_cont", "0.25"),
            Self::P75Exact => ("percentile_cont", "0.75"),
            Self::P90Exact => ("percentile_cont", "0.90"),
            Self::P95Exact => ("percentile_cont", "0.95"),
            Self::MedianApprox => ("approx_percentile_cont", "0.5"),
            Self::P25Approx => ("approx_percentile_cont", "0.25"),
            Self::P75Approx => ("approx_percentile_cont", "0.75"),
            Self::P90Approx => ("approx_percentile_cont", "0.90"),
            Self::P95Approx => ("approx_percentile_cont", "0.95"),
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum AggregateBucket {
    Day,
    Week,
    Month,
    Quarter,
    Year,
    DayPart,
    WeekPart,
    MonthPart,
    QuarterPart,
    YearPart,
}

impl AggregateBucket {
    pub fn label(self) -> &'static str {
        match self {
            Self::Day | Self::DayPart => "Day",
            Self::Week | Self::WeekPart => "Week",
            Self::Month | Self::MonthPart => "Month",
            Self::Quarter | Self::QuarterPart => "Quarter",
            Self::Year | Self::YearPart => "Year",
        }
    }

    fn date_part(self) -> Option<DatePart> {
        Some(match self {
            Self::DayPart => DatePart::Day,
            Self::WeekPart => DatePart::Week,
            Self::MonthPart => DatePart::Month,
            Self::QuarterPart => DatePart::Quarter,
            Self::YearPart => DatePart::Year,
            _ => return None,
        })
    }

    fn expression(self, name: &str, kind: &DataType) -> Result<String> {
        let field = identifier(name);
        let period = self.label();
        if self.date_part().is_some() {
            ensure!(
                matches!(
                    scalar_type(kind),
                    DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _)
                ),
                "This field does not support time grouping: {name}"
            );
            return Ok(format!("date_part('{period}', {field})"));
        }
        Ok(match scalar_type(kind) {
            DataType::Date32 | DataType::Date64 => format!(
                "CAST(date_trunc('{period}', arrow_cast({field}, 'Timestamp(Millisecond, None)')) AS DATE)"
            ),
            DataType::Timestamp(_, _) => format!("date_trunc('{period}', {field})"),
            _ => anyhow::bail!("This field does not support time grouping: {name}"),
        })
    }

    // Invoke the same Arrow-backed kernel and use the same casts as the SQL
    // plan. In particular Timestamp time zones and DST affect both paths.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn apply(self, values: &ArrayRef) -> Result<ArrayRef> {
        let kind = scalar_type(values.data_type());
        if let Some(part) = self.date_part() {
            let values = if values.data_type() == kind {
                values.clone()
            } else {
                cast(values.as_ref(), kind)?
            };
            return Ok(date_part(values.as_ref(), part)?);
        }
        let is_date = matches!(kind, DataType::Date32 | DataType::Date64);
        let input_kind = if is_date {
            DataType::Timestamp(TimeUnit::Millisecond, None)
        } else {
            kind.clone()
        };
        let values = if values.data_type() == &input_kind {
            values.clone()
        } else {
            cast(values.as_ref(), &input_kind)?
        };
        let rows = values.len();
        let field = Arc::new(Field::new("value", input_kind, true));
        let result = date_trunc()
            .invoke_with_args(ScalarFunctionArgs {
                args: vec![
                    ColumnarValue::Scalar(ScalarValue::Utf8(Some(self.label().into()))),
                    ColumnarValue::Array(values),
                ],
                arg_fields: vec![
                    Arc::new(Field::new("period", DataType::Utf8, false)),
                    field.clone(),
                ],
                number_rows: rows,
                return_field: field,
                config_options: Arc::default(),
            })?
            .into_array(rows)?;
        if is_date {
            Ok(cast(result.as_ref(), &DataType::Date32)?)
        } else {
            Ok(result)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateMetric {
    pub field: Option<String>,
    pub op: AggregateOp,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AggregateRequest {
    pub group_by: Vec<String>,
    pub group_buckets: Vec<(String, AggregateBucket)>,
    pub metrics: Vec<AggregateMetric>,
}

#[derive(Clone, Debug)]
pub struct AggregateResult {
    pub columns: Vec<ColumnInfo>,
    pub data: ArrowTable,
    pub group_by: Vec<String>,
    pub group_buckets: Vec<(String, AggregateBucket)>,
    pub input_rows: usize,
    pub elapsed_ms: u128,
}

impl AggregateResult {
    pub(crate) fn group_dimensions(&self) -> impl Iterator<Item = (&str, Option<AggregateBucket>)> {
        group_dimensions(&self.group_by, &self.group_buckets)
    }

    pub(crate) fn group_count(&self) -> usize {
        self.group_dimensions().count()
    }
}

fn group_dimensions<'a>(
    group_by: &'a [String],
    group_buckets: &'a [(String, AggregateBucket)],
) -> impl Iterator<Item = (&'a str, Option<AggregateBucket>)> {
    let mut buckets_by_field = HashMap::<_, Vec<_>>::new();
    for (name, bucket) in group_buckets {
        buckets_by_field
            .entry(name.as_str())
            .or_default()
            .push(*bucket);
    }
    group_by.iter().flat_map(move |name| {
        let name = name.as_str();
        let buckets = buckets_by_field.remove(name).unwrap_or_default();
        let exact = buckets.is_empty().then_some((name, None));
        exact
            .into_iter()
            .chain(buckets.into_iter().map(move |bucket| (name, Some(bucket))))
    })
}

pub fn supported_ops(data_type: &str) -> Vec<AggregateOp> {
    use AggregateOp::*;
    match crate::filtering::field_kind(data_type) {
        FilterFieldKind::Number => vec![
            Count,
            NUnique,
            Sum,
            Mean,
            Min,
            Max,
            MissingRatio,
            StdDevSample,
            StdDevPopulation,
            MedianExact,
            P25Exact,
            P75Exact,
            P90Exact,
            P95Exact,
            MedianApprox,
            P25Approx,
            P75Approx,
            P90Approx,
            P95Approx,
        ],
        FilterFieldKind::Text | FilterFieldKind::Boolean => vec![Count, NUnique, MissingRatio],
        FilterFieldKind::Date | FilterFieldKind::DateTime | FilterFieldKind::Time => {
            vec![Count, NUnique, Min, Max, MissingRatio]
        }
        _ if matches!(
            data_type.trim().to_ascii_uppercase().as_str(),
            "BLOB" | "BINARY" | "VARBINARY" | "UUID" | "JSON" | "NULL"
        ) =>
        {
            vec![Count, NUnique, MissingRatio]
        }
        _ => Vec::new(),
    }
}

pub fn supports_group(data_type: &str) -> bool {
    !supported_ops(data_type).is_empty()
}

pub fn supports_bucket(data_type: &str) -> bool {
    matches!(
        crate::filtering::field_kind(data_type),
        FilterFieldKind::Date | FilterFieldKind::DateTime
    )
}

pub(crate) struct AggregatePlan {
    select: String,
    compact_input: Option<String>,
    group_count: usize,
    pub labels: Vec<String>,
}

impl AggregatePlan {
    pub(crate) fn sql(&self, predicate: &str) -> String {
        let mut input = "aggregation_input".to_owned();
        if !predicate.is_empty() {
            input.push_str(" WHERE ");
            input.push_str(predicate);
        }
        if let Some(compact) = &self.compact_input {
            input = format!(
                "(SELECT {compact} FROM {input} GROUP BY {}) AS __pv_distinct_groups",
                group_ordinals(self.group_count + 1),
            );
        }
        let mut sql = format!("SELECT {} FROM {input}", self.select);
        if self.group_count > 0 {
            sql.push_str(" GROUP BY ");
            sql.push_str(&group_ordinals(self.group_count));
            sql.push_str(" ORDER BY ");
            sql.push_str(
                &(1..=self.group_count)
                    .map(|position| format!("{position} ASC NULLS LAST"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        sql
    }
}

fn group_ordinals(count: usize) -> String {
    (1..=count)
        .map(|position| position.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn scalar_type(kind: &DataType) -> &DataType {
    match kind {
        DataType::Dictionary(_, value) => scalar_type(value),
        DataType::RunEndEncoded(_, value) => scalar_type(value.data_type()),
        _ => kind,
    }
}

/// One Text distinct argument can be computed with a second GROUP BY.
/// The ordinary Utf8 accumulator otherwise allocates an independent string
/// hash table and buffer per group. Keep mergeable companion measures here.
fn compact_text_nunique<'a>(request: &'a AggregateRequest, schema: &Schema) -> Option<&'a str> {
    use AggregateOp::*;
    if request.group_by.is_empty()
        || request.metrics.iter().any(|metric| {
            !matches!(
                metric.op,
                CountRows | Count | NUnique | Sum | Min | Max | MissingRatio
            )
        })
    {
        return None;
    }
    let mut unique_fields = request
        .metrics
        .iter()
        .filter(|metric| metric.op == NUnique)
        .filter_map(|metric| metric.field.as_deref());
    let field = unique_fields.next()?;
    (unique_fields.all(|name| name == field)
        && schema.field_with_name(field).is_ok_and(|field| {
            matches!(
                field.data_type(),
                DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
            )
        }))
    .then_some(field)
}

fn unique_label(label: String, used: &mut HashSet<String>) -> String {
    if used.insert(label.to_ascii_lowercase()) {
        return label;
    }
    for suffix in 2.. {
        let candidate = format!("{label} ({suffix})");
        if used.insert(candidate.to_ascii_lowercase()) {
            return candidate;
        }
    }
    unreachable!()
}

/// DF 54's ordinary integer/Decimal SUM accumulators use wrapping addition.
/// Widen exact values before accumulation and prove the widest possible sum of
/// this snapshot fits, using schema precision and row count only. This avoids
/// scanning all values in Rust or silently wrapping i64/u64/Decimal sums.
fn exact_numeric_argument(
    name: &str,
    kind: &DataType,
    rows: usize,
    op: AggregateOp,
) -> Result<String> {
    let quoted = identifier(name);
    let (precision, scale) = match scalar_type(kind) {
        DataType::Int8 => (3u8, 0i8),
        DataType::Int16 => (5, 0),
        DataType::Int32 => (10, 0),
        DataType::Int64 => (19, 0),
        DataType::UInt8 => (3, 0),
        DataType::UInt16 => (5, 0),
        DataType::UInt32 => (10, 0),
        DataType::UInt64 => (20, 0),
        DataType::Decimal32(p, s)
        | DataType::Decimal64(p, s)
        | DataType::Decimal128(p, s)
        | DataType::Decimal256(p, s) => (*p, *s),
        DataType::Float16 | DataType::Float32 | DataType::Float64 => return Ok(quoted),
        _ => anyhow::bail!("The selected field does not support numeric aggregation: {name}"),
    };
    // No values will be cast or accumulated. Keep empty global summaries valid
    // even for a 76-digit source whose nonempty worst-case mean is unsafe.
    if rows == 0 {
        return Ok(format!("arrow_cast({quoted}, 'Decimal256(76,0)')"));
    }
    let target_scale = i16::from(scale).max(0);
    let source_digits = u32::from(precision) + u32::try_from(target_scale - i16::from(scale))?;
    let growth = rows
        .saturating_sub(1)
        .checked_ilog10()
        .map_or(0, |digits| digits + 1);
    let digits = source_digits + growth;
    let mean_growth = |maximum: u32| {
        if op == AggregateOp::Mean {
            (u32::try_from(target_scale).unwrap() + 4).min(maximum)
                - u32::try_from(target_scale).unwrap()
        } else {
            0
        }
    };
    let width = if target_scale <= 38 && digits + mean_growth(38) <= 38 {
        128
    } else {
        256
    };
    let maximum = if width == 128 { 38 } else { 76 };
    ensure!(
        target_scale <= maximum as i16 && digits + mean_growth(maximum) <= maximum,
        "Decimal aggregation cannot safely preserve this precision for the current number of rows. Reduce the imported row range or use a lower-precision field."
    );
    Ok(format!(
        "arrow_cast({quoted}, 'Decimal{width}({maximum},{target_scale})')"
    ))
}

pub(crate) fn plan(
    request: &AggregateRequest,
    columns: &[ColumnInfo],
    schema: &Schema,
    rows: usize,
) -> Result<AggregatePlan> {
    ensure!(
        !request.metrics.is_empty(),
        "Choose at least one aggregation."
    );
    let available = columns
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect::<HashMap<_, _>>();
    let mut groups = HashSet::new();
    for name in &request.group_by {
        ensure!(groups.insert(name), "Duplicate grouping field: {name}");
    }
    let mut labels = Vec::new();
    let mut used_labels = HashSet::new();
    let mut selections = Vec::new();
    let mut buckets = HashSet::new();
    for (name, bucket) in &request.group_buckets {
        ensure!(
            groups.contains(name),
            "Time grouping field is not selected: {name}"
        );
        ensure!(
            buckets.insert((name, *bucket)),
            "Duplicate time grouping dimension: {name} ({})",
            bucket.label()
        );
    }
    let distinct_field = compact_text_nunique(request, schema);
    let mut compact_input = Vec::new();
    let count_rows = if distinct_field.is_some() {
        "sum(__pv_input_count)"
    } else {
        "count(*)"
    };
    for (index, (name, bucket)) in
        group_dimensions(&request.group_by, &request.group_buckets).enumerate()
    {
        let column = available
            .get(name)
            .with_context(|| format!("Grouping field is not loaded: {name}"))?;
        ensure!(
            supports_group(&column.data_type),
            "Grouping is not supported for this Data type: {}",
            column.data_type
        );
        let (expression, label) = if let Some(bucket) = bucket {
            ensure!(
                supports_bucket(&column.data_type),
                "This field does not support time grouping: {name}"
            );
            (
                bucket.expression(name, schema.field_with_name(name)?.data_type())?,
                format!("{name} ({})", bucket.label()),
            )
        } else {
            (identifier(name), name.to_owned())
        };
        let alias = identifier(&format!("__pv_group_{index}"));
        let selection = format!("{expression} AS {alias}");
        if distinct_field.is_some() {
            compact_input.push(selection);
            selections.push(alias);
        } else {
            selections.push(selection);
        }
        labels.push(unique_label(label, &mut used_labels));
    }
    let group_count = labels.len();
    if let Some(distinct_field) = distinct_field {
        compact_input.push(format!(
            "{} AS __pv_distinct_value",
            identifier(distinct_field)
        ));
        compact_input.push("count(*) AS __pv_input_count".into());
    }
    for (index, metric) in request.metrics.iter().enumerate() {
        let mut merge = |aggregate: &str, argument: &str, combine: &str| {
            let expression = format!("{aggregate}({argument})");
            if distinct_field.is_some() {
                let alias = format!("__pv_compact_metric_{index}");
                compact_input.push(format!("{expression} AS {alias}"));
                format!("{combine}({alias})")
            } else {
                expression
            }
        };
        let (expression, label) = if metric.op == AggregateOp::CountRows {
            ensure!(metric.field.is_none(), "Count rows does not take a field.");
            (count_rows.to_owned(), "Count rows".to_owned())
        } else {
            let name = metric
                .field
                .as_deref()
                .context("Choose an aggregation field.")?;
            let column = available
                .get(name)
                .with_context(|| format!("Aggregation field is not loaded: {name}"))?;
            ensure!(
                supported_ops(&column.data_type).contains(&metric.op),
                "{} is not supported for this Data type: {}",
                metric.op.label(),
                column.data_type
            );
            let argument = if matches!(metric.op, AggregateOp::Sum | AggregateOp::Mean) {
                exact_numeric_argument(
                    name,
                    schema.field_with_name(name)?.data_type(),
                    rows,
                    metric.op,
                )?
            } else {
                identifier(name)
            };
            let expression = match metric.op {
                AggregateOp::Count => merge("count", &argument, "sum"),
                AggregateOp::NUnique if distinct_field.is_some() => {
                    "count(__pv_distinct_value)".into()
                }
                AggregateOp::NUnique => format!("count(DISTINCT {argument})"),
                AggregateOp::Sum => merge("sum", &argument, "sum"),
                AggregateOp::Mean => format!("avg({argument})"),
                AggregateOp::Min => merge("min", &argument, "min"),
                AggregateOp::Max => merge("max", &argument, "max"),
                AggregateOp::MissingRatio => {
                    let count = merge("count", &argument, "sum");
                    format!("CAST(({count_rows} - {count}) AS DOUBLE) / nullif({count_rows}, 0)")
                }
                AggregateOp::StdDevSample => format!("stddev(CAST({argument} AS DOUBLE))"),
                AggregateOp::StdDevPopulation => format!("stddev_pop(CAST({argument} AS DOUBLE))"),
                AggregateOp::CountRows => unreachable!(),
                percentile => {
                    let (function, rank) = percentile.percentile().context("Unknown percentile")?;
                    format!("{function}(CAST({argument} AS DOUBLE), {rank})")
                }
            };
            (expression, format!("{}({name})", metric.op.label()))
        };
        labels.push(unique_label(label, &mut used_labels));
        selections.push(format!(
            "{expression} AS {}",
            identifier(&format!("__pv_metric_{index}"))
        ));
    }
    // An inexpensive DF count also verifies replayed pure predicates select
    // exactly the immutable displayed snapshot's row count. It is not exposed.
    selections.push(format!("{count_rows} AS __pv_input_count"));
    Ok(AggregatePlan {
        select: selections.join(", "),
        compact_input: distinct_field.map(|_| compact_input.join(", ")),
        group_count,
        labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregation_operators_are_schema_bound_and_nested_types_are_excluded() {
        use AggregateOp::*;
        for name in ["BIGINT", "UBIGINT", "DOUBLE", "DECIMAL(38,18)"] {
            assert!(supported_ops(name).contains(&Sum));
            assert!(supported_ops(name).contains(&StdDevSample));
            assert!(supported_ops(name).contains(&P90Exact));
        }
        assert!(supports_bucket("DATE"));
        assert!(supports_bucket("TIMESTAMP_NS"));
        assert!(!supports_bucket("TIME"));
        assert!(!supports_bucket("VARCHAR"));
        for name in ["VARCHAR", "BOOLEAN", "BLOB", "UUID", "JSON"] {
            assert_eq!(supported_ops(name), [Count, NUnique, MissingRatio]);
        }
        for name in ["DATE", "TIMESTAMP_NS", "TIME"] {
            assert_eq!(
                supported_ops(name),
                [Count, NUnique, Min, Max, MissingRatio]
            );
        }
        for name in [
            "BIGINT[]",
            "STRUCT(\"x\" BIGINT)",
            "MAP(VARCHAR, BIGINT)",
            "INTERVAL",
            "UNION(\"x\" BIGINT)",
        ] {
            assert!(!supports_group(name));
            assert!(supported_ops(name).is_empty());
        }
    }

    #[test]
    fn exact_numeric_sums_and_means_widen_without_silent_wrapping() -> Result<()> {
        assert!(
            exact_numeric_argument("big", &DataType::Int64, 2, AggregateOp::Sum)?
                .contains("Decimal128(38,0)")
        );
        assert!(
            exact_numeric_argument("big", &DataType::UInt64, 2, AggregateOp::Mean)?
                .contains("Decimal128(38,0)")
        );
        assert!(
            exact_numeric_argument(
                "amount",
                &DataType::Decimal128(38, 18),
                200000,
                AggregateOp::Sum
            )?
            .contains("Decimal256(76,18)")
        );
        assert!(
            exact_numeric_argument(
                "amount",
                &DataType::Decimal128(38, -10),
                200000,
                AggregateOp::Mean
            )?
            .contains("Decimal256(76,0)")
        );
        assert!(
            exact_numeric_argument("amount", &DataType::Decimal256(76, 0), 2, AggregateOp::Sum)
                .is_err()
        );
        assert!(
            exact_numeric_argument("amount", &DataType::Decimal256(76, 0), 0, AggregateOp::Mean)
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn date_bucket_preserves_dates_nulls_and_monday_week_start() -> Result<()> {
        use arrow::array::Date32Array;
        let days: ArrayRef = Arc::new(Date32Array::from(vec![
            Some(0),
            Some(1),
            Some(31),
            Some(91),
            None,
        ]));
        for (bucket, expected) in [
            (
                AggregateBucket::Day,
                [Some(0), Some(1), Some(31), Some(91), None],
            ),
            (
                AggregateBucket::Week,
                [Some(-3), Some(-3), Some(25), Some(88), None],
            ),
            (
                AggregateBucket::Month,
                [Some(0), Some(0), Some(31), Some(90), None],
            ),
            (
                AggregateBucket::Quarter,
                [Some(0), Some(0), Some(0), Some(90), None],
            ),
            (
                AggregateBucket::Year,
                [Some(0), Some(0), Some(0), Some(0), None],
            ),
        ] {
            let result = bucket.apply(&days)?;
            let result = result.as_any().downcast_ref::<Date32Array>().unwrap();
            assert_eq!(result.iter().collect::<Vec<_>>(), expected, "{bucket:?}");
        }
        Ok(())
    }

    #[test]
    fn grouped_text_nunique_keeps_null_groups_counts_and_exact_companion_sums() -> Result<()> {
        use crate::engine::{Engine, QueryRequest};
        use AggregateOp::*;
        use arrow::array::{Array, Float64Array, Int64Array, StringArray};
        use std::sync::atomic::AtomicBool;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("distinct.parquet");
        fixture(&path)?;
        let mut engine = Engine::open(&[path])?;
        let loaded = engine.query(&QueryRequest::default())?;
        let request = AggregateRequest {
            group_by: vec!["group".into()],
            group_buckets: Vec::new(),
            metrics: vec![
                AggregateMetric {
                    field: None,
                    op: CountRows,
                },
                metric("text", NUnique),
                metric("text", Count),
                metric("text", MissingRatio),
                metric("id", Sum),
            ],
        };
        assert_eq!(
            compact_text_nunique(&request, &loaded.data.schema()),
            Some("text")
        );
        let plan = plan(
            &request,
            &loaded.columns,
            &loaded.data.schema(),
            loaded.row_count(),
        )?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let context = crate::datafusion_session::new_context(
                datafusion::prelude::SessionConfig::new(),
                Arc::default(),
            );
            context.register_batch("aggregation_input", loaded.data.raw_batches()[0].clone())?;
            let plan = context.sql(&plan.sql("")).await?.into_optimized_plan()?;
            let plan = plan.display_indent().to_string();
            assert_eq!(plan.matches("Aggregate:").count(), 2, "{plan}");
            assert!(!plan.contains("DISTINCT"), "{plan}");
            Ok::<_, anyhow::Error>(())
        })?;
        let result = engine.aggregate_snapshot_cancellable(
            &loaded.snapshot_id,
            &request,
            &AtomicBool::new(false),
        )?;
        let batch = result.data.materialize_range(0..5)?;
        let groups = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(
            groups.iter().collect::<Vec<_>>(),
            [Some("A"), Some("B"), Some("C"), Some("NULL"), None]
        );
        for (index, expected) in [
            (1, [2, 2, 2, 1, 1]),
            (2, [1, 1, 0, 1, 0]),
            (3, [2, 2, 0, 1, 0]),
        ] {
            let values = batch
                .column(index)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            assert_eq!(values.values().as_ref(), &expected);
        }
        let ratios = batch
            .column(4)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(ratios.values().as_ref(), &[0., 0., 1., 0., 1.]);
        for (row, expected) in ["1", "6", "11", "7", "3"].iter().enumerate() {
            assert!(!batch.column(5).is_null(row));
            assert_eq!(
                arrow::util::display::array_value_to_string(batch.column(5).as_ref(), row)?,
                *expected
            );
        }
        Ok(())
    }

    #[test]
    fn float_statistics_interpolate_ranks_and_keep_empty_groups_null() -> Result<()> {
        use crate::engine::{Engine, QueryRequest};
        use AggregateOp::*;
        use arrow::array::{Array, Float64Array};
        use std::sync::atomic::AtomicBool;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("statistics.parquet");
        fixture(&path)?;
        let mut engine = Engine::open(&[path])?;
        let request = AggregateRequest {
            metrics: vec![
                metric("text", MissingRatio),
                metric("id", StdDevSample),
                metric("id", StdDevPopulation),
                metric("id", P25Exact),
                metric("id", MedianExact),
                metric("id", P75Exact),
                metric("id", P90Exact),
                metric("id", P95Exact),
                metric("id", MedianApprox),
            ],
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        for (filter, expected) in [
            (
                "",
                vec![
                    0.375,
                    6_f64.sqrt(),
                    5.25_f64.sqrt(),
                    1.75,
                    3.5,
                    5.25,
                    6.3,
                    6.65,
                ],
            ),
            ("id = 0", vec![0., f64::NAN, 0., 0., 0., 0., 0., 0.]),
            ("id < 0", vec![f64::NAN; 8]),
        ] {
            let loaded = engine.query(&QueryRequest {
                filter: filter.into(),
                ..Default::default()
            })?;
            let result =
                engine.aggregate_snapshot_cancellable(&loaded.snapshot_id, &request, &cancel)?;
            let batch = result.data.materialize_range(0..1)?;
            for (column, expected) in batch.columns().iter().zip(expected) {
                let values = column.as_any().downcast_ref::<Float64Array>().unwrap();
                if expected.is_nan() {
                    assert!(values.is_null(0));
                } else {
                    // DF 54.1's exact rank selection uses an interpolation
                    // weight quantized to six fractional digits.
                    assert!(
                        (values.value(0) - expected).abs() <= 1.01e-6,
                        "{filter}: {} != {expected}",
                        values.value(0)
                    );
                }
            }
            let approximate = batch
                .column(8)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            if filter == "id < 0" {
                assert!(approximate.is_null(0));
            } else {
                let expected = if filter.is_empty() { 3.5 } else { 0. };
                assert!((approximate.value(0) - expected).abs() <= 1.);
            }
        }
        Ok(())
    }

    #[test]
    fn summary_uses_the_clipped_sorted_rows_of_a_top_n_snapshot() -> Result<()> {
        use crate::engine::{Engine, QueryRequest};
        use std::sync::atomic::AtomicBool;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("top-n-summary.parquet");
        fixture(&path)?;
        let mut engine = Engine::open(&[path])?;
        let loaded = engine.query(&QueryRequest {
            filter: "id >= 1".into(),
            sort: Some(("id".into(), true)),
            top_n: Some(3),
            ..Default::default()
        })?;
        let summary = engine.aggregate_snapshot_cancellable(
            &loaded.snapshot_id,
            &AggregateRequest {
                group_by: vec!["group".into()],
                metrics: vec![metric("id", AggregateOp::Sum)],
                ..Default::default()
            },
            &AtomicBool::new(false),
        )?;
        assert_eq!(summary.input_rows, 3);
        assert_eq!(summary.data.row_count(), 2);
        assert_eq!(summary.data.cell(0, 0).text, "C");
        assert_eq!(summary.data.cell(0, 1).text, "11");
        assert_eq!(summary.data.cell(1, 0).text, "NULL");
        assert_eq!(summary.data.cell(1, 1).text, "7");
        Ok(())
    }

    fn fixture(path: &std::path::Path) -> Result<()> {
        use arrow::{
            array::{
                ArrayRef, BooleanArray, Date32Array, Decimal128Array, Int64Array, ListArray,
                StringArray, StructArray, UInt64Array,
            },
            datatypes::{Field, Int64Type},
            record_batch::RecordBatch,
        };
        use std::{fs::File, sync::Arc};
        let exact = 9007199254740993123456789012345678i128;
        let id = Arc::new(Int64Array::from_iter_values(0..8));
        let nested = Arc::new(StructArray::from(vec![(
            Arc::new(Field::new("x", DataType::Int64, false)),
            id.clone() as ArrayRef,
        )]));
        let list = Arc::new(ListArray::from_iter_primitive::<Int64Type, _, _>(
            (0..8).map(|i| Some(vec![Some(i)])),
        ));
        let batch = RecordBatch::try_from_iter(vec![
            (
                "group",
                Arc::new(StringArray::from(vec![
                    Some("A"),
                    Some("A"),
                    Some("B"),
                    None,
                    Some("B"),
                    Some("C"),
                    Some("C"),
                    Some("NULL"),
                ])) as ArrayRef,
            ),
            ("id", id as ArrayRef),
            (
                "n.with\"quote",
                Arc::new(Int64Array::from(vec![
                    Some(i64::MAX),
                    Some(1),
                    None,
                    Some(-5),
                    Some(5),
                    None,
                    None,
                    Some(0),
                ])) as ArrayRef,
            ),
            (
                "uint",
                Arc::new(UInt64Array::from(vec![
                    Some(u64::MAX),
                    Some(1),
                    None,
                    Some(0),
                    Some(5),
                    None,
                    None,
                    Some(0),
                ])) as ArrayRef,
            ),
            (
                "amount",
                Arc::new(
                    Decimal128Array::from(vec![
                        Some(exact),
                        Some(exact + 1),
                        None,
                        Some(0),
                        Some(1),
                        None,
                        None,
                        Some(0),
                    ])
                    .with_precision_and_scale(38, 18)?,
                ) as ArrayRef,
            ),
            (
                "text",
                Arc::new(StringArray::from(vec![
                    Some("x"),
                    Some("x"),
                    Some("y"),
                    None,
                    Some("y"),
                    None,
                    None,
                    Some("z"),
                ])) as ArrayRef,
            ),
            (
                "flag",
                Arc::new(BooleanArray::from(vec![
                    Some(true),
                    Some(false),
                    Some(true),
                    None,
                    Some(true),
                    Some(false),
                    None,
                    Some(true),
                ])) as ArrayRef,
            ),
            (
                "day",
                Arc::new(Date32Array::from(vec![
                    Some(20000),
                    Some(20001),
                    None,
                    Some(20003),
                    Some(20004),
                    None,
                    None,
                    Some(20007),
                ])) as ArrayRef,
            ),
            ("nested", nested as ArrayRef),
            ("list", list as ArrayRef),
        ])?;
        let properties = parquet::file::properties::WriterProperties::builder()
            .set_max_row_group_row_count(Some(3))
            .build();
        let mut writer = parquet::arrow::ArrowWriter::try_new(
            File::create(path)?,
            batch.schema(),
            Some(properties),
        )?;
        writer.write(&batch)?;
        writer.close()?;
        Ok(())
    }

    fn metric(field: &str, op: AggregateOp) -> AggregateMetric {
        AggregateMetric {
            field: Some(field.into()),
            op,
        }
    }

    #[test]
    fn dataframe_grouping_counts_nulls_and_exact_numeric_values_without_overflow() -> Result<()> {
        use crate::engine::{Engine, QueryRequest};
        use std::sync::atomic::AtomicBool;
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("aggregate.parquet");
        fixture(&source)?;
        let mut engine = Engine::open(&[source])?;
        let displayed = engine.query(&QueryRequest::default())?;
        let result = engine.aggregate_snapshot_cancellable(
            &displayed.snapshot_id,
            &AggregateRequest {
                group_by: vec!["group".into()],
                group_buckets: Vec::new(),
                metrics: vec![
                    AggregateMetric {
                        field: None,
                        op: AggregateOp::CountRows,
                    },
                    metric("text", AggregateOp::Count),
                    metric("text", AggregateOp::NUnique),
                    metric("n.with\"quote", AggregateOp::Sum),
                    metric("n.with\"quote", AggregateOp::Mean),
                    metric("uint", AggregateOp::Sum),
                    metric("amount", AggregateOp::Sum),
                    metric("amount", AggregateOp::Mean),
                    metric("day", AggregateOp::Min),
                    metric("day", AggregateOp::Max),
                    metric("flag", AggregateOp::NUnique),
                ],
            },
            &AtomicBool::new(false),
        )?;
        assert_eq!(result.input_rows, 8);
        assert_eq!(result.data.row_count(), 5);
        let row = (0..result.data.row_count())
            .find(|&row| result.data.cell(row, 0).text == "A")
            .unwrap();
        assert_eq!(result.data.cell(row, 1).text, "2");
        assert_eq!(result.data.cell(row, 2).text, "2");
        assert_eq!(result.data.cell(row, 3).text, "1");
        assert_eq!(result.data.cell(row, 4).text, "9223372036854775808");
        assert_eq!(result.data.cell(row, 5).text, "4611686018427387904.0000");
        assert_eq!(result.data.cell(row, 6).text, "18446744073709551616");
        assert_eq!(
            result.data.cell(row, 7).text,
            "18014398509481986.246913578024691357"
        );
        assert_eq!(
            result.data.cell(row, 8).text,
            "9007199254740993.1234567890123456785000"
        );
        assert_eq!(result.columns[9].data_type, "DATE");
        assert_eq!(result.data.cell(row, 11).text, "2");
        let all_null = (0..result.data.row_count())
            .find(|&row| result.data.cell(row, 0).text == "C")
            .unwrap();
        assert_eq!(result.data.cell(all_null, 1).text, "2");
        assert_eq!(result.data.cell(all_null, 2).text, "0");
        assert_eq!(result.data.cell(all_null, 3).text, "0");
        for column in [4, 5, 6, 7, 8, 9, 10] {
            assert!(result.data.is_null(all_null, column));
        }
        assert_eq!(
            (0..result.data.row_count())
                .filter(|&row| result.data.is_null(row, 0))
                .count(),
            1
        );
        assert_eq!(
            (0..result.data.row_count())
                .filter(
                    |&row| !result.data.is_null(row, 0) && result.data.cell(row, 0).text == "NULL"
                )
                .count(),
            1
        );
        assert!(result.data.source_row_number(0).is_none());
        Ok(())
    }

    #[test]
    fn aggregation_uses_filtered_ranged_snapshot_after_source_removal_and_keeps_main_result()
    -> Result<()> {
        use crate::engine::{Engine, ExportFormat, QueryRequest};
        use std::sync::atomic::AtomicBool;
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source.parquet");
        fixture(&source)?;
        let mut engine = Engine::open(std::slice::from_ref(&source))?;
        let displayed = engine.query(&QueryRequest {
            offset: 1,
            limit: 5,
            filter: "id >= 2".into(),
            sort: Some(("id".into(), true)),
            ..Default::default()
        })?;
        let current = engine.query(&QueryRequest {
            selected_columns: vec!["id".into()],
            offset: 1,
            limit: 5,
            ..Default::default()
        })?;
        std::fs::remove_file(source)?;
        let request = AggregateRequest {
            group_by: vec!["group".into()],
            group_buckets: Vec::new(),
            metrics: vec![
                AggregateMetric {
                    field: None,
                    op: AggregateOp::CountRows,
                },
                metric("n.with\"quote", AggregateOp::Sum),
            ],
        };
        let result = engine.aggregate_snapshot_cancellable(
            &displayed.snapshot_id,
            &request,
            &AtomicBool::new(false),
        )?;
        assert_eq!(result.input_rows, 4);
        assert_eq!(result.data.row_count(), 3);
        let b = (0..result.data.row_count())
            .find(|&row| result.data.cell(row, 0).text == "B")
            .unwrap();
        assert_eq!(result.data.cell(b, 1).text, "2");
        assert_eq!(result.data.cell(b, 2).text, "5");
        assert!(
            engine
                .aggregate_snapshot_cancellable(
                    &current.snapshot_id,
                    &request,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert!(
            engine
                .aggregate_snapshot_cancellable(
                    &displayed.snapshot_id,
                    &request,
                    &AtomicBool::new(true)
                )
                .is_err()
        );
        let target = directory.path().join("current.json");
        engine.export_current(&target, ExportFormat::Json)?;
        let rows: Vec<serde_json::Value> = serde_json::from_reader(std::fs::File::open(target)?)?;
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], serde_json::json!({"id":1}));
        assert_eq!(displayed.source_row_number(0), Some(6));
        Ok(())
    }

    #[test]
    fn aggregation_supports_multi_keys_empty_inputs_and_duplicate_metric_labels() -> Result<()> {
        use crate::engine::{Engine, QueryRequest};
        use std::sync::atomic::AtomicBool;
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("aggregate.parquet");
        fixture(&source)?;
        let mut engine = Engine::open(&[source])?;
        let displayed = engine.query(&QueryRequest::default())?;
        let request = AggregateRequest {
            group_by: vec!["group".into(), "flag".into()],
            group_buckets: Vec::new(),
            metrics: vec![
                metric("text", AggregateOp::Count),
                metric("text", AggregateOp::Count),
                metric("id", AggregateOp::NUnique),
            ],
        };
        let result = engine.aggregate_snapshot_cancellable(
            &displayed.snapshot_id,
            &request,
            &AtomicBool::new(false),
        )?;
        assert_eq!(result.data.row_count(), 7);
        assert_eq!(result.columns[2].name, "Count(text)");
        assert_eq!(result.columns[3].name, "Count(text) (2)");
        for (field, op) in [
            ("text", AggregateOp::Sum),
            ("nested", AggregateOp::Count),
            ("list", AggregateOp::NUnique),
        ] {
            assert!(
                engine
                    .aggregate_snapshot_cancellable(
                        &displayed.snapshot_id,
                        &AggregateRequest {
                            group_by: vec![],
                            group_buckets: Vec::new(),
                            metrics: vec![metric(field, op)]
                        },
                        &AtomicBool::new(false)
                    )
                    .is_err()
            );
        }
        let empty = engine.query(&QueryRequest {
            filter: "id < 0".into(),
            ..Default::default()
        })?;
        let count = AggregateMetric {
            field: None,
            op: AggregateOp::CountRows,
        };
        let result = engine.aggregate_snapshot_cancellable(
            &empty.snapshot_id,
            &AggregateRequest {
                group_by: vec![],
                group_buckets: Vec::new(),
                metrics: vec![
                    count.clone(),
                    metric("text", AggregateOp::Count),
                    metric("id", AggregateOp::Sum),
                ],
            },
            &AtomicBool::new(false),
        )?;
        assert_eq!(result.input_rows, 0);
        assert_eq!(result.data.row_count(), 1);
        assert_eq!(result.data.cell(0, 0).text, "0");
        assert_eq!(result.data.cell(0, 1).text, "0");
        assert!(result.data.is_null(0, 2));
        let grouped = engine.aggregate_snapshot_cancellable(
            &empty.snapshot_id,
            &AggregateRequest {
                group_by: vec!["group".into()],
                group_buckets: Vec::new(),
                metrics: vec![count],
            },
            &AtomicBool::new(false),
        )?;
        assert_eq!(grouped.data.row_count(), 0);
        Ok(())
    }
}
