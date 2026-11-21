//! Plot only the published aggregate rows, with work bounded by the plot settings.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow::{
    array::{Array, PrimitiveArray},
    compute::cast,
    datatypes::{ArrowPrimitiveType, DataType, Float64Type, Int64Type},
};
use gpui::{Div, SharedString, div, prelude::*, px, relative, rgb};

use crate::{
    aggregation::{AggregateMetric, AggregateResult},
    controls,
    i18n::t,
    icons,
    theme::Colors,
};

const LABEL_WIDTH: f32 = 0.34;
const VALUE_WIDTH: f32 = 0.25;

pub(crate) enum AggregateCharts {
    Disabled,
    BarsDisabled,
    Limited(usize),
    Empty,
    Plots {
        groups: Arc<Vec<ChartLabel>>,
        total: usize,
        plots: Vec<AggregatePlot>,
    },
}

pub(crate) struct AggregatePlot {
    metric: AggregateMetric,
    marks: Arc<Vec<Mark>>,
    axis: Option<Axis>,
}

struct Mark {
    label: ChartLabel,
    position: Option<f32>,
}

#[derive(Clone)]
pub(crate) struct ChartLabel {
    text: SharedString,
    full: SharedString,
}

impl ChartLabel {
    fn new(full: SharedString, numeric: bool) -> Self {
        let text = numeric
            .then(|| compact_number(&full))
            .flatten()
            .filter(|text| text != full.as_str())
            .map_or_else(|| full.clone(), SharedString::from);
        Self { text, full }
    }
}

struct Axis {
    first: ChartLabel,
    last: ChartLabel,
    zero: Option<f32>,
}

impl AggregateCharts {
    pub(crate) fn new(
        result: &AggregateResult,
        metrics: &[AggregateMetric],
        row_limit: usize,
        category_bars: usize,
        cancel: &AtomicBool,
    ) -> Option<Self> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let total = result.data.row_count();
        if row_limit == 0 {
            return Some(Self::Disabled);
        }
        if category_bars == 0 {
            return Some(Self::BarsDisabled);
        }
        if total > row_limit {
            return Some(Self::Limited(row_limit));
        }
        if total == 0 {
            return Some(Self::Empty);
        }
        let visible = total.min(category_bars);
        let group_count = result.group_count();
        let groups = Arc::new(group_labels(result, visible, cancel)?);
        let mut plots = Vec::with_capacity(metrics.len());
        for (index, metric) in metrics.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            plots.push(AggregatePlot::new(
                result,
                metric.clone(),
                group_count + index,
                visible,
                cancel,
            )?);
        }
        Some(Self::Plots {
            groups,
            total,
            plots,
        })
    }

    pub(crate) fn render(&self, dark: bool) -> Div {
        let colors = Colors::new(dark);
        match self {
            Self::Disabled => note(colors, t("Charts are disabled in Settings.")),
            Self::BarsDisabled => note(colors, t("Bar charts are disabled in Settings.")),
            Self::Limited(limit) => note(
                colors,
                t("Chart skipped. The current result exceeds the chart row limit of {limit}.")
                    .replace("{limit}", &limit.to_string()),
            ),
            Self::Empty => note(colors, t("No groups to plot.")),
            Self::Plots {
                groups,
                total,
                plots,
            } => div()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .when(groups.len() < *total, |el| {
                    el.child(note(
                        colors,
                        t("Showing the first {shown} of {total} groups.")
                            .replace("{shown}", &groups.len().to_string())
                            .replace("{total}", &total.to_string()),
                    ))
                })
                .children(
                    plots
                        .iter()
                        .enumerate()
                        .map(|(index, plot)| plot.render(index, groups, colors, dark)),
                ),
        }
    }
}

fn group_labels(
    result: &AggregateResult,
    rows: usize,
    cancel: &AtomicBool,
) -> Option<Vec<ChartLabel>> {
    let schema = result.data.schema();
    let numeric = result
        .group_dimensions()
        .enumerate()
        .map(|(index, (_, bucket))| {
            bucket.is_none() && schema.field(index).data_type().is_numeric()
        })
        .collect::<Vec<_>>();
    let mut labels: Vec<_> = (0..rows)
        .map(|row| {
            if row.is_multiple_of(1024) && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let parts = numeric.iter().enumerate().map(|(col, &numeric)| {
                let cell = result.data.display_text(row, col);
                let cell = if cell.is_empty() {
                    "\"\"".into()
                } else {
                    cell.into()
                };
                ChartLabel::new(cell, numeric)
            });
            let mut text = String::new();
            let mut full = String::new();
            let mut text_truncated = false;
            let mut full_truncated = false;
            for part in parts {
                if !text_truncated {
                    text_truncated = push_group_part(&mut text, &part.text);
                }
                if !full_truncated {
                    full_truncated = push_group_part(&mut full, &part.full);
                }
                if text_truncated && full_truncated {
                    break;
                }
            }
            let text: SharedString = text.into();
            let full = if full == text.as_str() {
                text.clone()
            } else {
                full.into()
            };
            Some(ChartLabel { text, full })
        })
        .collect::<Option<_>>()?;
    if !numeric.contains(&true) {
        return Some(labels);
    }
    // Rounded numeric labels are decorative. If two visible keys would share
    // a label, display the original values for both groups.
    let mut original = HashMap::with_capacity(labels.len());
    let mut ambiguous = HashSet::new();
    for label in &labels {
        if let Some(previous) = original.insert(&label.text, &label.full)
            && previous != &label.full
        {
            ambiguous.insert(label.text.clone());
        }
    }
    for label in &mut labels {
        if ambiguous.contains(&label.text) {
            label.text = label.full.clone();
        }
    }
    Some(labels)
}

fn push_group_part(label: &mut String, part: &str) -> bool {
    if !label.is_empty() {
        label.push_str(" · ");
    }
    label.push_str(part);
    if let Some((end, _)) = label.char_indices().nth(200) {
        label.truncate(end);
        label.push('…');
        return true;
    }
    false
}

/// Format the original Arrow display value so rounding does not first lose
/// precision in a Float64 conversion. The chart geometry is computed separately.
fn compact_number(value: &str) -> Option<String> {
    let negative = value.starts_with('-');
    let value = value.strip_prefix(['-', '+']).unwrap_or(value);
    let (mantissa, exponent) = value.split_once(['e', 'E']).unwrap_or((value, "0"));
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = integer.bytes().chain(fraction.bytes());
    if (integer.is_empty() && fraction.is_empty())
        || !digits.clone().all(|digit| digit.is_ascii_digit())
    {
        return None;
    }
    let Some(first) = digits.position(|digit| digit != b'0') else {
        return Some("0".to_owned());
    };
    let mut power = exponent
        .parse::<i32>()
        .ok()?
        .checked_add(integer.len() as i32 - first as i32 - 1)?;
    let mut scientific = !(-2..6).contains(&power);
    let digit_count = if scientific { 3 } else { (power + 3) as usize };
    let mut significand = [0u8; 8];
    let mut source = integer.bytes().chain(fraction.bytes()).skip(first);
    for digit in &mut significand[..digit_count] {
        *digit = source.next().map_or(0, |digit| digit - b'0');
    }
    let mut carry = source.next().is_some_and(|digit| digit >= b'5');
    for digit in significand[..digit_count].iter_mut().rev() {
        if !carry {
            break;
        }
        *digit += 1;
        carry = *digit == 10;
        if carry {
            *digit = 0;
        }
    }
    if carry {
        significand[0] = 1;
        power = power.checked_add(1)?;
        scientific |= power >= 6;
    }
    let mut digits = &significand[..digit_count];
    while let Some((0, remaining)) = digits.split_last() {
        digits = remaining;
    }
    let mut text = String::with_capacity(16);
    if negative {
        text.push('-');
    }
    let integer_len = if scientific { 1 } else { power + 1 };
    if integer_len <= 0 {
        text.push_str("0.");
        for _ in integer_len..0 {
            text.push('0');
        }
    }
    for position in 0..digits.len().max(integer_len.max(0) as usize) {
        if position > 0 && position as i32 == integer_len {
            text.push('.');
        }
        text.push(char::from(
            b'0' + digits.get(position).copied().unwrap_or(0),
        ));
    }
    if scientific {
        use std::fmt::Write;
        let _ = write!(text, "e{power}");
    }
    Some(text)
}

impl AggregatePlot {
    fn new(
        result: &AggregateResult,
        metric: AggregateMetric,
        col: usize,
        rows: usize,
        cancel: &AtomicBool,
    ) -> Option<Self> {
        let schema = result.data.schema();
        let kind = schema.fields().get(col).map(|field| field.data_type());
        let mut marks: Vec<_> = (0..rows)
            .map(|row| {
                if row.is_multiple_of(1024) && cancel.load(Ordering::Relaxed) {
                    return None;
                }
                Some(Mark {
                    label: ChartLabel::new(
                        result.data.display_text(row, col).into(),
                        kind.is_some_and(DataType::is_numeric),
                    ),
                    position: None,
                })
            })
            .collect::<Option<_>>()?;
        let axis = match kind {
            Some(kind) if kind.is_numeric() => {
                let values = bounded_values::<Float64Type>(result, col, rows, &DataType::Float64)
                    .unwrap_or_default();
                numeric_axis(&mut marks, values)
            }
            Some(
                DataType::Date32
                | DataType::Date64
                | DataType::Timestamp(_, _)
                | DataType::Time32(_)
                | DataType::Time64(_),
            ) => {
                let values = bounded_values::<Int64Type>(result, col, rows, &DataType::Int64)
                    .unwrap_or_default();
                temporal_axis(&mut marks, values)
            }
            _ => None,
        };
        Some(Self {
            metric,
            marks: Arc::new(marks),
            axis,
        })
    }

    fn render(
        &self,
        index: usize,
        groups: &Arc<Vec<ChartLabel>>,
        colors: Colors,
        dark: bool,
    ) -> Div {
        let operation = t(self.metric.op.label());
        let title = self.metric.field.as_ref().map_or_else(
            || operation.clone(),
            |field| format!("{operation}({field})"),
        );
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .p_2()
            .border_1()
            .border_color(rgb(colors.border))
            .rounded(px(5.))
            .bg(rgb(colors.surface))
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .whitespace_normal()
                    .child(title),
            )
            .map(|card| {
                let Some(axis) = &self.axis else {
                    return card.child(note(colors, t("No finite values to plot.")));
                };
                let groups = groups.clone();
                let marks = self.marks.clone();
                let zero = axis.zero;
                card.child(controls::chart_rows(
                    ("aggregation-plot-rows", index),
                    marks.len(),
                    move |row| {
                        let mark = &marks[row];
                        let group = &groups[row];
                        let group = if group.text.is_empty() {
                            ChartLabel::new(t("All records").into(), false)
                        } else {
                            group.clone()
                        };
                        div()
                            .id(("aggregation-plot-mark", row))
                            .min_w_0()
                            .flex()
                            .items_center()
                            .text_size(px(11.))
                            .tooltip(icons::tooltip(
                                format!("{}: {}", group.full, mark.label.full),
                                dark,
                            ))
                            .child(
                                div()
                                    .w(relative(LABEL_WIDTH))
                                    .flex_shrink_0()
                                    .pr_2()
                                    .truncate()
                                    .child(group.text),
                            )
                            .child(plot_mark(mark, zero, colors))
                            .child(
                                div()
                                    .w(relative(VALUE_WIDTH))
                                    .flex_shrink_0()
                                    .pl_2()
                                    .text_right()
                                    .truncate()
                                    .child(mark.label.text.clone()),
                            )
                    },
                ))
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .text_size(px(10.))
                        .text_color(rgb(colors.muted))
                        .child(div().w(relative(LABEL_WIDTH)).flex_shrink_0())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .gap_1()
                                .justify_between()
                                .border_t_1()
                                .border_color(rgb(colors.border))
                                .pt_1()
                                .child(
                                    div()
                                        .id(("aggregation-plot-minimum", index))
                                        .truncate()
                                        .tooltip(icons::tooltip(axis.first.full.clone(), dark))
                                        .child(axis.first.text.clone()),
                                )
                                .when(axis.first.text != axis.last.text, |el| {
                                    el.child(
                                        div()
                                            .id(("aggregation-plot-maximum", index))
                                            .text_right()
                                            .truncate()
                                            .tooltip(icons::tooltip(axis.last.full.clone(), dark))
                                            .child(axis.last.text.clone()),
                                    )
                                }),
                        )
                        .child(div().w(relative(VALUE_WIDTH)).flex_shrink_0()),
                )
            })
    }
}

fn plot_mark(mark: &Mark, zero: Option<f32>, colors: Colors) -> Div {
    div()
        .relative()
        .flex_1()
        .min_w_0()
        .h(px(14.))
        .when_some(zero, |el, zero| {
            el.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(zero))
                    .w(px(1.))
                    .bg(rgb(colors.input_border)),
            )
        })
        .when_some(mark.position, |el, position| {
            let shape = div()
                .absolute()
                .top(px(2.))
                .h(px(10.))
                .bg(rgb(colors.primary));
            el.child(match zero {
                Some(zero) => shape
                    .left(relative(position.min(zero)))
                    .w(relative((position - zero).abs()))
                    .rounded(px(2.)),
                None => shape
                    .left(relative(position))
                    .ml(px(-4.))
                    .w(px(8.))
                    .rounded_full(),
            })
        })
}

fn note(colors: Colors, text: String) -> Div {
    div()
        .text_size(px(11.))
        .text_color(rgb(colors.muted))
        .whitespace_normal()
        .child(text)
}

/// Aggregate results are unindexed RecordBatches in group order. Slice before
/// casting so numeric and temporal conversion visits only plotted rows.
fn bounded_values<T: ArrowPrimitiveType>(
    result: &AggregateResult,
    col: usize,
    rows: usize,
    kind: &DataType,
) -> Option<Vec<Option<T::Native>>> {
    let mut values = Vec::with_capacity(rows);
    for batch in result.data.raw_batches() {
        let len = batch.num_rows().min(rows - values.len());
        let slice = batch.column(col).slice(0, len);
        let cast = cast(slice.as_ref(), kind).ok()?;
        let array = cast.as_any().downcast_ref::<PrimitiveArray<T>>()?;
        values.extend(array.iter());
        if values.len() == rows {
            break;
        }
    }
    Some(values)
}

fn numeric_axis(marks: &mut [Mark], values: Vec<Option<f64>>) -> Option<Axis> {
    let finite = |value: Option<f64>| value.filter(|value| value.is_finite());
    let mut extrema = values
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(index, value)| finite(value).map(|value| (index, value)));
    let first = extrema.next()?;
    let (min, max) = extrema.fold((first, first), |(min, max), (index, value)| {
        (
            if value < min.1 { (index, value) } else { min },
            if value > max.1 { (index, value) } else { max },
        )
    });
    let lower = min.1.min(0.);
    let upper = max.1.max(0.);
    let position = |value| {
        let span = upper - lower;
        if span == 0. {
            0.
        } else if span.is_finite() {
            ((value - lower) / span) as f32
        } else {
            let scale = lower.abs().max(upper.abs());
            ((value / scale - lower / scale) / (upper / scale - lower / scale)) as f32
        }
    };
    for (mark, value) in marks.iter_mut().zip(values) {
        mark.position = finite(value).map(position);
    }
    Some(Axis {
        first: if lower == 0. {
            ChartLabel::new("0".into(), false)
        } else {
            marks[min.0].label.clone()
        },
        last: if upper == 0. {
            ChartLabel::new("0".into(), false)
        } else {
            marks[max.0].label.clone()
        },
        zero: Some(position(0.)),
    })
}

fn temporal_axis(marks: &mut [Mark], values: Vec<Option<i64>>) -> Option<Axis> {
    let mut extrema = values
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(index, value)| value.map(|value| (index, value)));
    let first = extrema.next()?;
    let (min, max) = extrema.fold((first, first), |(min, max), (index, value)| {
        (
            if value < min.1 { (index, value) } else { min },
            if value > max.1 { (index, value) } else { max },
        )
    });
    let span = i128::from(max.1) - i128::from(min.1);
    for (mark, value) in marks.iter_mut().zip(values) {
        mark.position = value.map(|value| {
            if span == 0 {
                0.5
            } else {
                ((i128::from(value) - i128::from(min.1)) as f64 / span as f64) as f32
            }
        });
    }
    Some(Axis {
        first: marks[min.0].label.clone(),
        last: marks[max.0].label.clone(),
        zero: None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::{
        array::{ArrayRef, Date32Array, Float64Array, Int32Array, Int64Array, StringArray},
        record_batch::RecordBatch,
    };

    use super::*;
    use crate::{
        aggregation::{AggregateBucket, AggregateOp},
        arrow_types::columns_from_schema,
        columnar::ArrowTable,
    };

    #[test]
    fn bar_limit_can_disable_aggregate_charts_or_include_every_group() -> anyhow::Result<()> {
        let values = Arc::new(Int32Array::from_iter_values(0..150)) as ArrayRef;
        let batch = RecordBatch::try_from_iter([("group", values.clone()), ("value", values)])?;
        let columns = columns_from_schema(&batch.schema());
        let result = AggregateResult {
            columns: columns.clone(),
            data: ArrowTable::new(columns, vec![batch])?,
            group_by: vec!["group".into()],
            group_buckets: vec![],
            input_rows: 150,
            elapsed_ms: 0,
        };
        let metrics = [AggregateMetric {
            field: None,
            op: AggregateOp::CountRows,
        }];
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            AggregateCharts::new(&result, &metrics, usize::MAX, 0, &cancel),
            Some(AggregateCharts::BarsDisabled)
        ));
        for (limit, expected) in [(1, 1), (120, 120), (usize::MAX, 150)] {
            let Some(AggregateCharts::Plots { groups, plots, .. }) =
                AggregateCharts::new(&result, &metrics, usize::MAX, limit, &cancel)
            else {
                panic!("expected plots")
            };
            assert_eq!(groups.len(), expected);
            assert_eq!(plots[0].marks.len(), expected);
            assert_eq!(
                groups[expected - 1].text.as_str(),
                (expected - 1).to_string()
            );
        }
        Ok(())
    }

    #[test]
    fn aggregate_charts_follow_metric_order_and_bound_rows_across_batches() -> anyhow::Result<()> {
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec!["", "east", "west", "south"])),
            Arc::new(Int32Array::from(vec![2024, 2024, 2025, 2025])),
            Arc::new(Int32Array::from(vec![1, 2, 1, 2])),
            Arc::new(Float64Array::from(vec![
                Some(-f64::MAX),
                Some(f64::MAX),
                None,
                Some(f64::INFINITY),
            ])),
            Arc::new(Int64Array::from(vec![2, 3, 4, 5])),
            Arc::new(Date32Array::from(vec![Some(2), Some(4), None, Some(6)])),
        ];
        let batch = RecordBatch::try_from_iter(
            [
                "Area",
                "Date (Year)",
                "Date (Month)",
                "Sum(Number)",
                "Count rows",
                "Minimum(Date)",
            ]
            .into_iter()
            .zip(arrays),
        )?;
        let columns = columns_from_schema(&batch.schema());
        let result = AggregateResult {
            columns: columns.clone(),
            data: ArrowTable::new(columns, vec![batch.slice(0, 2), batch.slice(2, 2)])?,
            group_by: vec!["Area".into(), "Date".into()],
            group_buckets: vec![
                ("Date".into(), AggregateBucket::YearPart),
                ("Date".into(), AggregateBucket::MonthPart),
            ],
            input_rows: 14,
            elapsed_ms: 0,
        };
        let metrics = [
            AggregateMetric {
                field: Some("Number".into()),
                op: AggregateOp::Sum,
            },
            AggregateMetric {
                field: None,
                op: AggregateOp::CountRows,
            },
            AggregateMetric {
                field: Some("Date".into()),
                op: AggregateOp::Min,
            },
        ];
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            AggregateCharts::new(&result, &metrics, 0, 3, &cancel),
            Some(AggregateCharts::Disabled)
        ));
        assert!(matches!(
            AggregateCharts::new(&result, &metrics, 3, 3, &cancel),
            Some(AggregateCharts::Limited(3))
        ));
        let Some(AggregateCharts::Plots {
            groups,
            total,
            plots,
        }) = AggregateCharts::new(&result, &metrics, 4, 3, &cancel)
        else {
            panic!("Expected aggregate plots")
        };
        assert_eq!(total, 4);
        assert_eq!(
            groups
                .iter()
                .map(|label| label.text.as_str())
                .collect::<Vec<_>>(),
            ["\"\" · 2024 · 1", "east · 2024 · 2", "west · 2025 · 1"]
        );
        for (plot, metric) in plots.iter().zip(metrics) {
            assert_eq!(plot.metric, metric);
            assert_eq!(plot.marks.len(), 3);
        }
        assert_eq!(plots[0].axis.as_ref().unwrap().zero, Some(0.5));
        assert_eq!(
            plots[0].axis.as_ref().unwrap().first.text.as_str(),
            "-1.8e308"
        );
        assert_eq!(
            plots[0].axis.as_ref().unwrap().last.text.as_str(),
            "1.8e308"
        );
        assert_eq!(plots[0].marks[0].position, Some(0.));
        assert_eq!(plots[0].marks[1].position, Some(1.));
        assert_eq!(plots[0].marks[2].position, None);
        assert_eq!(plots[1].marks[2].label.text.as_str(), "4");
        assert_eq!(plots[2].axis.as_ref().unwrap().zero, None);
        assert_eq!(plots[2].marks[0].position, Some(0.));
        assert_eq!(plots[2].marks[1].position, Some(1.));
        assert_eq!(plots[2].marks[2].position, None);
        cancel.store(true, Ordering::Relaxed);
        assert!(AggregateCharts::new(&result, &[], 4, 3, &cancel).is_none());
        Ok(())
    }

    #[test]
    fn chart_numbers_round_exact_decimal_digits_across_notation_thresholds() {
        for (original, display) in [
            ("1234.567", "1234.57"),
            ("1.00499999999999999999999999999", "1"),
            ("1.00500000000000000000000000001", "1.01"),
            ("-0.01499", "-0.01"),
            ("0.00999999999999999999", "1e-2"),
            ("999999.995000000000000000001", "1e6"),
            ("1.2349999999999999999999e+40", "1.23e40"),
            ("1.2350000000000000000001e+40", "1.24e40"),
            ("4.9406564584124654e-324", "4.94e-324"),
            ("-0.0000", "0"),
        ] {
            assert_eq!(
                compact_number(original).as_deref(),
                Some(display),
                "{original}"
            );
        }
        for special in ["NULL", "NaN", "-inf", "inf"] {
            assert_eq!(compact_number(special), None);
        }
    }

    #[test]
    fn numeric_group_labels_keep_keys_that_would_round_to_the_same_text() -> anyhow::Result<()> {
        let batch = RecordBatch::try_from_iter([
            (
                "key",
                Arc::new(Int64Array::from(vec![1_000_001, 1_000_002, 2_540_000])) as ArrayRef,
            ),
            (
                "label",
                Arc::new(StringArray::from(vec!["1.005"; 3])) as ArrayRef,
            ),
        ])?;
        let columns = columns_from_schema(&batch.schema());
        let result = AggregateResult {
            columns: columns.clone(),
            data: ArrowTable::new(columns, vec![batch])?,
            group_by: vec!["key".into(), "label".into()],
            group_buckets: vec![],
            input_rows: 3,
            elapsed_ms: 0,
        };
        let labels = group_labels(&result, 3, &AtomicBool::new(false)).unwrap();
        assert_eq!(labels[0].text.as_str(), "1000001 · 1.005");
        assert_eq!(labels[1].text.as_str(), "1000002 · 1.005");
        assert_eq!(labels[2].text.as_str(), "2.54e6 · 1.005");
        assert_eq!(labels[2].full.as_str(), "2540000 · 1.005");
        Ok(())
    }
}
