//! Bounded, cancellable statistics over the displayed Arrow view.
use crate::{columnar::ArrowTable, engine::QueryResult};
use anyhow::{Context, Result, ensure};
use arrow::{
    array::{Array, AsArray, PrimitiveArray, UnionArray},
    compute::{max, min},
    datatypes::{ArrowNativeTypeOp, ArrowNumericType, DataType, TimeUnit, ToByteSlice, *},
    util::display::{array_value_to_string, lexical_to_string},
};
use std::{
    cmp::{Ordering, Reverse},
    collections::{BinaryHeap, HashMap},
    hash::Hash,
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering::Relaxed},
};

const SCAN_BLOCK: usize = 16_384;
const AUTO_MAX_CATEGORIES: usize = 32;
const CATEGORY_SCRATCH_BYTES: usize = 256 * 1024 * 1024;
const CATEGORY_MEMORY_ERROR: &str =
    "Not enough memory to calculate exact Category counts. Reduce the loaded range.";
pub(crate) const DEFAULT_CATEGORY_CHART_BARS: usize = 8;
const DENSITY_POINTS: usize = 96;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProfileMode {
    #[default]
    Auto,
    Category,
    Text,
}

#[derive(Debug)]
pub(crate) struct ProfileStats {
    pub rows: usize,
    pub nulls: usize,
    pub chart_limited: bool,
    pub details: ProfileDetails,
}

#[derive(Debug)]
pub(crate) enum ProfileDetails {
    Category {
        distinct: usize,
        common: Vec<(String, usize)>,
    },
    Number {
        range: Option<(String, String)>,
        quantiles: Option<[String; 5]>,
        non_finite: usize,
        density: Option<Density>,
    },
    Boolean {
        trues: usize,
        falses: usize,
    },
    Temporal(Option<(String, String)>),
    NullsOnly,
}

#[derive(Debug)]
pub(crate) struct Density {
    pub points: Vec<f64>,
    pub labels: [String; 3],
}

struct Column<'a> {
    table: &'a ArrowTable,
    index: usize,
    cancel: &'a AtomicBool,
}

impl<'a> Column<'a> {
    fn check_cancel(&self) -> Result<()> {
        ensure!(!self.cancel.load(Relaxed), "Operation cancelled");
        Ok(())
    }

    fn rows(&self) -> usize {
        self.table.row_count()
    }

    fn value(&self, row: usize) -> (&'a dyn Array, usize) {
        let (array, index) = self
            .table
            .array(row, self.index)
            .expect("validated profile row");
        resolved(array, index)
    }

    fn batches(&self) -> impl Iterator<Item = &'a dyn Array> {
        self.table
            .raw_batches()
            .iter()
            .map(|batch| batch.column(self.index).as_ref())
    }

    fn visit(&self, mut visit: impl FnMut(&'a dyn Array, usize, usize)) -> Result<()> {
        self.try_visit(|array, index, row| {
            visit(array, index, row);
            Ok(true)
        })
    }

    /// The callback returns false after it has enough information to stop.
    fn try_visit(
        &self,
        mut visit: impl FnMut(&'a dyn Array, usize, usize) -> Result<bool>,
    ) -> Result<()> {
        self.check_cancel()?;
        if self.table.has_row_indices() {
            for start in (0..self.rows()).step_by(SCAN_BLOCK) {
                self.check_cancel()?;
                for row in start..self.rows().min(start.saturating_add(SCAN_BLOCK)) {
                    let (array, index) = self.value(row);
                    if !visit(array, index, row)? {
                        return Ok(());
                    }
                }
            }
        } else {
            let mut offset = 0;
            for array in self.batches() {
                let indirect = is_indirect(array.data_type());
                for start in (0..array.len()).step_by(SCAN_BLOCK) {
                    self.check_cancel()?;
                    for row in start..array.len().min(start.saturating_add(SCAN_BLOCK)) {
                        let (value, index) = if indirect {
                            resolved(array, row)
                        } else {
                            (array, row)
                        };
                        if !visit(value, index, offset + row)? {
                            return Ok(());
                        }
                    }
                }
                offset += array.len();
            }
        }
        Ok(())
    }

    fn null_count(&self, encoded_type: &DataType) -> Result<usize> {
        if !self.table.has_row_indices() && !is_indirect(encoded_type) {
            let mut count = 0;
            for array in self.batches() {
                self.check_cancel()?;
                count += array.logical_null_count();
            }
            return Ok(count);
        }
        let mut count = 0;
        self.visit(|array, index, _| count += usize::from(is_null(array, index)))?;
        Ok(count)
    }
}

fn is_indirect(kind: &DataType) -> bool {
    matches!(
        kind,
        DataType::Dictionary(..) | DataType::RunEndEncoded(..) | DataType::Union(..)
    )
}

fn resolved(array: &dyn Array, row: usize) -> (&dyn Array, usize) {
    arrow::array::downcast_dictionary_array! {
        array => {
            return array.key(row).map_or((array as &dyn Array, row), |key| {
                resolved(array.values().as_ref(), key)
            });
        }
        _ => {}
    }
    arrow::array::downcast_run_array! {
        array => {
            return resolved(array.values().as_ref(), array.get_physical_index(row));
        }
        _ => {}
    }
    if let Some(union) = array.as_any().downcast_ref::<UnionArray>() {
        return resolved(
            union.child(union.type_id(row)).as_ref(),
            union.value_offset(row),
        );
    }
    (array, row)
}

fn is_null(array: &dyn Array, row: usize) -> bool {
    matches!(array.data_type(), DataType::Null) || array.is_null(row)
}

fn value_type(kind: &DataType) -> &DataType {
    match kind {
        DataType::Dictionary(_, values) => value_type(values),
        DataType::RunEndEncoded(_, values) => value_type(values.data_type()),
        _ => kind,
    }
}

fn is_string(kind: &DataType) -> bool {
    matches!(
        kind,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
    )
}

pub(crate) fn profile_modes(result: &QueryResult, column: usize) -> bool {
    result
        .data
        .schema()
        .fields()
        .get(column)
        .is_some_and(|field| {
            let kind = value_type(field.data_type());
            field.metadata().get("ARROW:extension:name").is_none()
                && (is_string(kind) || kind.is_numeric())
        })
}

pub(crate) fn profile_has_text_mode(result: &QueryResult, column: usize) -> bool {
    result
        .data
        .schema()
        .fields()
        .get(column)
        .is_some_and(|field| {
            field.metadata().get("ARROW:extension:name").is_none()
                && is_string(value_type(field.data_type()))
        })
}

pub(crate) fn calculate_profile(
    result: &QueryResult,
    index: usize,
    mode: ProfileMode,
    chart_row_limit: usize,
    category_chart_bars: usize,
    cancel: &AtomicBool,
) -> Result<ProfileStats> {
    let column = Column {
        table: &result.data,
        index,
        cancel,
    };
    column.check_cancel()?;
    let schema = column.table.schema();
    let Some(field) = schema.fields().get(index) else {
        anyhow::bail!("Profile field is outside the current result");
    };
    let encoded = field.data_type();
    let kind = value_type(encoded);
    let direct = !column.table.has_row_indices() && !is_indirect(encoded);
    let rows = column.rows();
    let chart_allowed = chart_row_limit != 0 && rows <= chart_row_limit;
    let category_chart_bars = if chart_allowed {
        category_chart_bars
    } else {
        0
    };
    let extension = field.metadata().get("ARROW:extension:name");
    let (nulls, details) = if extension.is_some() || mode == ProfileMode::Text {
        (column.null_count(encoded)?, ProfileDetails::NullsOnly)
    } else if is_string(kind) {
        let detect_text = mode == ProfileMode::Auto && !matches!(encoded, DataType::Dictionary(..));
        category(
            &column,
            category_chart_bars,
            string_value,
            |value, _| preview_label(value),
            detect_text.then_some(|value: &str| value.len() > 96 || value.contains(['\n', '\r'])),
            encoded,
        )?
    } else {
        macro_rules! number {
            ($t:ty, $finite:expr, $float:expr) => {
                if mode == ProfileMode::Category {
                    numeric_category::<$t, { size_of::<<$t as ArrowPrimitiveType>::Native>() }>(
                        &column,
                        category_chart_bars,
                        encoded,
                    )?
                } else {
                    numeric::<$t>(&column, kind, direct, chart_allowed, $finite, $float)?
                }
            };
        }
        macro_rules! temporal {
            ($t:ty) => {
                temporal::<$t>(&column, kind, direct)?
            };
        }
        match kind {
            DataType::Int8 => number!(Int8Type, None, None),
            DataType::Int16 => number!(Int16Type, None, None),
            DataType::Int32 => number!(Int32Type, None, None),
            DataType::Int64 => number!(Int64Type, None, None),
            DataType::UInt8 => number!(UInt8Type, None, None),
            DataType::UInt16 => number!(UInt16Type, None, None),
            DataType::UInt32 => number!(UInt32Type, None, None),
            DataType::UInt64 => number!(UInt64Type, None, None),
            DataType::Float16 => number!(
                Float16Type,
                Some(|value| value.is_finite()),
                Some(|value| value.to_f64())
            ),
            DataType::Float32 => number!(Float32Type, Some(f32::is_finite), Some(f64::from)),
            DataType::Float64 => number!(Float64Type, Some(f64::is_finite), Some(|value| value)),
            DataType::Decimal32(..) => number!(Decimal32Type, None, None),
            DataType::Decimal64(..) => number!(Decimal64Type, None, None),
            DataType::Decimal128(..) => number!(Decimal128Type, None, None),
            DataType::Decimal256(..) => number!(Decimal256Type, None, None),
            DataType::Date32 => temporal!(Date32Type),
            DataType::Date64 => temporal!(Date64Type),
            DataType::Timestamp(TimeUnit::Second, _) => temporal!(TimestampSecondType),
            DataType::Timestamp(TimeUnit::Millisecond, _) => temporal!(TimestampMillisecondType),
            DataType::Timestamp(TimeUnit::Microsecond, _) => temporal!(TimestampMicrosecondType),
            DataType::Timestamp(TimeUnit::Nanosecond, _) => temporal!(TimestampNanosecondType),
            DataType::Time32(TimeUnit::Second) => temporal!(Time32SecondType),
            DataType::Time32(TimeUnit::Millisecond) => temporal!(Time32MillisecondType),
            DataType::Time64(TimeUnit::Microsecond) => temporal!(Time64MicrosecondType),
            DataType::Time64(TimeUnit::Nanosecond) => temporal!(Time64NanosecondType),
            DataType::Boolean => booleans(&column, direct)?,
            _ => (column.null_count(encoded)?, ProfileDetails::NullsOnly),
        }
    };
    column.check_cancel()?;
    let chart_limited = !chart_allowed
        && matches!(
            details,
            ProfileDetails::Category { .. }
                | ProfileDetails::Number { .. }
                | ProfileDetails::Boolean { .. }
        );
    Ok(ProfileStats {
        rows,
        nulls,
        chart_limited,
        details,
    })
}

fn string_value(array: &dyn Array, row: usize) -> Option<&str> {
    if is_null(array, row) {
        return None;
    }
    match array.data_type() {
        DataType::Utf8 => Some(array.as_string::<i32>().value(row)),
        DataType::LargeUtf8 => Some(array.as_string::<i64>().value(row)),
        DataType::Utf8View => Some(array.as_string_view().value(row)),
        _ => None,
    }
}

fn preview_label(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".into();
    }
    let mut chars = value.chars();
    let mut label: String = chars
        .by_ref()
        .take(48)
        .map(|character| match character {
            '\n' | '\r' => '↵',
            '\t' => '⇥',
            character if character.is_control() => '�',
            character => character,
        })
        .collect();
    if chars.next().is_some() {
        label.push('…');
    }
    label
}

#[derive(Default)]
struct Frequency {
    count: usize,
    row: usize,
}

fn insert_category<K: Hash + Eq, V>(
    categories: &mut HashMap<K, V>,
    key: K,
    value: V,
    entry_limit: usize,
) -> Result<()> {
    ensure!(categories.len() < entry_limit, CATEGORY_MEMORY_ERROR);
    if categories.len() == categories.capacity() {
        categories.try_reserve(1).context(CATEGORY_MEMORY_ERROR)?;
    }
    categories.insert(key, value);
    Ok(())
}

fn category_leaders<K: Ord>(
    frequencies: HashMap<K, Frequency>,
    max_bars: usize,
    cancel: &AtomicBool,
) -> Result<Vec<(K, Frequency)>> {
    // Keep the least frequent retained category at the root. Ties retain the
    // smaller key first, independent of hash-table iteration order.
    let mut leaders = BinaryHeap::with_capacity(max_bars.min(frequencies.len()));
    for (index, (key, frequency)) in frequencies.into_iter().enumerate() {
        if index.is_multiple_of(SCAN_BLOCK) {
            ensure!(!cancel.load(Relaxed), "Operation cancelled");
        }
        let entry = (Reverse(frequency.count), key, frequency.row);
        if leaders.len() < max_bars {
            leaders.push(entry);
        } else if let Some(mut last) = leaders.peek_mut()
            && entry < *last
        {
            *last = entry;
        }
    }
    Ok(leaders
        .into_sorted_vec()
        .into_iter()
        .map(|(Reverse(count), key, row)| (key, Frequency { count, row }))
        .collect())
}

fn count_categories<'a, K: Copy + Hash + Eq, V: Default>(
    column: &Column<'a>,
    key: impl Fn(&'a dyn Array, usize) -> Option<K>,
    detect_text: Option<fn(K) -> bool>,
    encoded: &DataType,
    count: impl Fn(&mut V, usize),
) -> Result<(usize, Option<HashMap<K, V>>)> {
    // Reserve headroom for bucket slack, control bytes, and the old allocation
    // during growth. Strings are borrowed from Arrow and never materialized.
    let entry_limit = CATEGORY_SCRATCH_BYTES / (size_of::<(K, V)>() + 1) / 4;
    let mut categories = HashMap::new();
    let mut nulls = 0;
    let mut text = false;
    column.try_visit(|array, index, row| {
        let Some(value) = key(array, index) else {
            nulls += 1;
            return Ok(true);
        };
        if detect_text.is_some_and(|is_text| is_text(value)) {
            text = true;
            return Ok(false);
        }
        if let Some(existing) = categories.get_mut(&value) {
            count(existing, row);
        } else {
            if detect_text.is_some() && categories.len() == AUTO_MAX_CATEGORIES {
                text = true;
                return Ok(false);
            }
            let mut new = V::default();
            count(&mut new, row);
            insert_category(&mut categories, value, new, entry_limit)?;
        }
        Ok(true)
    })?;
    if text {
        Ok((column.null_count(encoded)?, None))
    } else {
        Ok((nulls, Some(categories)))
    }
}

fn category<'a, K: Copy + Hash + Eq + Ord>(
    column: &Column<'a>,
    max_bars: usize,
    key: impl Fn(&'a dyn Array, usize) -> Option<K>,
    label: impl Fn(K, usize) -> String,
    detect_text: Option<fn(K) -> bool>,
    encoded: &DataType,
) -> Result<(usize, ProfileDetails)> {
    if max_bars == 0 {
        let (nulls, categories) =
            count_categories(column, key, detect_text, encoded, |_: &mut (), _| {})?;
        let details = categories.map_or(ProfileDetails::NullsOnly, |categories| {
            ProfileDetails::Category {
                distinct: categories.len(),
                common: Vec::new(),
            }
        });
        return Ok((nulls, details));
    }
    let (nulls, frequencies) = count_categories(
        column,
        key,
        detect_text,
        encoded,
        |count: &mut Frequency, row| {
            if count.count == 0 {
                count.row = row;
            }
            count.count += 1;
        },
    )?;
    let Some(frequencies) = frequencies else {
        return Ok((nulls, ProfileDetails::NullsOnly));
    };
    let distinct = frequencies.len();
    let common = category_leaders(frequencies, max_bars, column.cancel)?
        .into_iter()
        .enumerate()
        .map(|(rank, (value, frequency))| {
            if rank.is_multiple_of(SCAN_BLOCK) {
                column.check_cancel()?;
            }
            Ok((label(value, frequency.row), frequency.count))
        })
        .collect::<Result<_>>()?;
    Ok((nulls, ProfileDetails::Category { distinct, common }))
}

fn numeric_category<T: ArrowNumericType, const WIDTH: usize>(
    column: &Column<'_>,
    max_bars: usize,
    encoded: &DataType,
) -> Result<(usize, ProfileDetails)>
where
    T::Native: ArrowNativeTypeOp + ToByteSlice,
{
    category(
        column,
        max_bars,
        |array, row| {
            if is_null(array, row) {
                return None;
            }
            let value = array.as_primitive::<T>().value(row);
            let value = if value == T::Native::ZERO {
                T::Native::ZERO
            } else {
                value
            };
            let mut key = [0; WIDTH];
            key.copy_from_slice(value.to_byte_slice());
            Some(key)
        },
        |_, row| {
            let (array, index) = column.value(row);
            array_value_to_string(array, index).unwrap_or_default()
        },
        None,
        encoded,
    )
}

fn booleans(column: &Column<'_>, direct: bool) -> Result<(usize, ProfileDetails)> {
    let mut nulls = 0;
    let mut trues = 0;
    if direct {
        for array in column.batches() {
            let array = array.as_boolean();
            for start in (0..array.len()).step_by(SCAN_BLOCK) {
                column.check_cancel()?;
                let part = array.slice(start, SCAN_BLOCK.min(array.len() - start));
                nulls += part.null_count();
                trues += part.true_count();
            }
        }
    } else {
        column.visit(|array, row, _| {
            if is_null(array, row) {
                nulls += 1;
            } else if array.as_boolean().value(row) {
                trues += 1;
            }
        })?;
    }
    Ok((
        nulls,
        ProfileDetails::Boolean {
            trues,
            falses: column.rows() - nulls - trues,
        },
    ))
}

struct Extrema<N> {
    nulls: usize,
    non_finite: usize,
    range: Option<(N, N)>,
}

impl<N: ArrowNativeTypeOp> Extrema<N> {
    fn include(&mut self, low: N, high: N) {
        let (old_low, old_high) = self.range.unwrap_or((low, high));
        self.range = Some((
            if low.compare(old_low) == Ordering::Less {
                low
            } else {
                old_low
            },
            if high.compare(old_high) == Ordering::Greater {
                high
            } else {
                old_high
            },
        ));
    }
}

fn extrema<T: ArrowNumericType>(
    column: &Column<'_>,
    direct: bool,
    finite: Option<fn(T::Native) -> bool>,
) -> Result<Extrema<T::Native>>
where
    T::Native: ArrowNativeTypeOp,
{
    let mut output = Extrema {
        nulls: 0,
        non_finite: 0,
        range: None,
    };
    if direct {
        for array in column.batches() {
            let array = array.as_primitive::<T>();
            for start in (0..array.len()).step_by(SCAN_BLOCK) {
                column.check_cancel()?;
                let part = array.slice(start, SCAN_BLOCK.min(array.len() - start));
                output.nulls += part.null_count();
                if let Some((low, high)) = min(&part).zip(max(&part)) {
                    // Float extrema use Arrow's total order. If both endpoints
                    // are finite, every non-null value is finite. Only chunks
                    // containing infinities or NaN need the typed fallback.
                    if let Some(finite) = finite.filter(|finite| !finite(low) || !finite(high)) {
                        for value in part.iter().flatten() {
                            if finite(value) {
                                output.include(value, value);
                            } else {
                                output.non_finite += 1;
                            }
                        }
                    } else {
                        output.include(low, high);
                    }
                }
            }
        }
    } else {
        column.visit(|array, row, _| {
            if is_null(array, row) {
                output.nulls += 1;
                return;
            }
            let value = array.as_primitive::<T>().value(row);
            if finite.is_none_or(|finite| finite(value)) {
                output.include(value, value);
            } else {
                output.non_finite += 1;
            }
        })?;
    }
    Ok(output)
}

fn native_array<T: ArrowNumericType>(values: Vec<T::Native>, kind: &DataType) -> PrimitiveArray<T> {
    PrimitiveArray::<T>::new(values.into(), None).with_data_type(kind.clone())
}

fn native_range<T: ArrowNumericType>(
    range: (T::Native, T::Native),
    kind: &DataType,
) -> Result<(String, String)> {
    let values = native_array::<T>(vec![range.0, range.1], kind);
    Ok((
        array_value_to_string(&values, 0)?,
        array_value_to_string(&values, 1)?,
    ))
}

fn temporal<T: ArrowNumericType>(
    column: &Column<'_>,
    kind: &DataType,
    direct: bool,
) -> Result<(usize, ProfileDetails)>
where
    T::Native: ArrowNativeTypeOp,
{
    let summary = extrema::<T>(column, direct, None)?;
    let range = summary
        .range
        .map(|range| native_range::<T>(range, kind))
        .transpose()?;
    Ok((summary.nulls, ProfileDetails::Temporal(range)))
}

fn scan_numbers<T: ArrowNumericType>(
    column: &Column<'_>,
    direct: bool,
    finite: Option<fn(T::Native) -> bool>,
    mut visit: impl FnMut(T::Native),
) -> Result<()> {
    let mut accept = |value: T::Native| {
        if finite.is_none_or(|finite| finite(value)) {
            visit(value);
        }
    };
    if direct {
        for array in column.batches() {
            let array = array.as_primitive::<T>();
            for start in (0..array.len()).step_by(SCAN_BLOCK) {
                column.check_cancel()?;
                let part = array.slice(start, SCAN_BLOCK.min(array.len() - start));
                if part.null_count() == 0 {
                    part.values().iter().copied().for_each(&mut accept);
                } else {
                    part.iter().flatten().for_each(&mut accept);
                }
            }
        }
    } else {
        column.visit(|array, row, _| {
            if !is_null(array, row) {
                accept(array.as_primitive::<T>().value(row));
            }
        })?;
    }
    Ok(())
}

fn rank_key<N: ToByteSlice>(value: N, signed: bool, floating: bool) -> [u8; 32] {
    let bytes = value.to_byte_slice();
    let mut key = [0; 32];
    // All supported targets are little-endian, as are Arrow's Decimal256 limbs.
    let width = bytes.len();
    for (output, &input) in key[..width].iter_mut().zip(bytes.iter().rev()) {
        *output = input;
    }
    if floating && key[0] & 128 != 0 {
        for byte in &mut key[..width] {
            *byte = !*byte;
        }
    } else if signed {
        key[0] ^= 128;
    }
    key
}

fn native_integer<N: ToByteSlice>(value: N, unsigned: bool) -> i256 {
    let bytes = value.to_byte_slice();
    let extension = if !unsigned && bytes[bytes.len() - 1] & 128 != 0 {
        255
    } else {
        0
    };
    let mut extended = [extension; 32];
    extended[..bytes.len()].copy_from_slice(bytes);
    i256::from_le_bytes(extended)
}

fn to_f64(value: i256) -> f64 {
    let (low, high) = value.wrapping_abs().to_parts();
    let magnitude = (high as f64).mul_add(2f64.powi(128), low as f64);
    if value.is_negative() {
        -magnitude
    } else {
        magnitude
    }
}

enum DensityDomain<N> {
    Float {
        origin: f64,
        span: f64,
        scale: Option<f64>,
        convert: fn(N) -> f64,
    },
    Integer {
        origin: i256,
        span: f64,
        unsigned: bool,
    },
}

impl<N: ToByteSlice> DensityDomain<N> {
    fn new(low: N, high: N, unsigned: bool, as_float: Option<fn(N) -> f64>) -> Self {
        if let Some(convert) = as_float {
            let origin = convert(low);
            let high = convert(high);
            let span = high - origin;
            if span.is_finite() {
                Self::Float {
                    origin,
                    span,
                    scale: None,
                    convert,
                }
            } else {
                let scale = origin.abs().max(high.abs()).max(f64::MIN_POSITIVE);
                let origin = origin / scale;
                Self::Float {
                    origin,
                    span: high / scale - origin,
                    scale: Some(scale),
                    convert,
                }
            }
        } else {
            let origin = native_integer(low, unsigned);
            Self::Integer {
                origin,
                span: to_f64(native_integer(high, unsigned) - origin),
                unsigned,
            }
        }
    }

    fn position(&self, value: N) -> f64 {
        match *self {
            Self::Float { span: 0., .. } => 0.5,
            Self::Float {
                origin,
                span,
                scale,
                convert,
            } => {
                let value = convert(value);
                (scale.map_or(value, |scale| value / scale) - origin) / span
            }
            Self::Integer {
                origin,
                span,
                unsigned,
            } => to_f64(native_integer(value, unsigned) - origin) / span,
        }
    }
}

fn numeric<T: ArrowNumericType>(
    column: &Column<'_>,
    kind: &DataType,
    direct: bool,
    chart_allowed: bool,
    finite: Option<fn(T::Native) -> bool>,
    as_float: Option<fn(T::Native) -> f64>,
) -> Result<(usize, ProfileDetails)>
where
    T::Native: ArrowNativeTypeOp,
{
    let summary = extrema::<T>(column, direct, finite)?;
    let valid = column.rows() - summary.nulls - summary.non_finite;
    let mut density = None;
    let mut quantiles = None;
    let range = if let Some((low, high)) = summary.range {
        let formatted_range = native_range::<T>((low, high), kind)?;
        let unsigned = kind.is_unsigned_integer();
        let mut bins = chart_allowed.then(DensityBins::new);
        let ranks = if low.is_eq(high) {
            if let Some(bins) = &mut bins {
                bins.fill_midpoint(valid);
            }
            [(low, high, 0); 5]
        } else {
            let key = |value| rank_key(value, !unsigned, as_float.is_some());
            let low_key = key(low);
            let high_key = key(high);
            let width = std::mem::size_of::<T::Native>();
            let prefix = low_key
                .iter()
                .zip(high_key.iter())
                .take_while(|(a, b)| a == b)
                .count();
            let mut density_pass = bins
                .as_mut()
                .map(|bins| (bins, DensityDomain::new(low, high, unsigned, as_float)));
            crate::profile_quantiles::percentile_ranks::<T::Native>(
                valid,
                width - prefix,
                |a, b| a.compare(*b),
                |visit| {
                    let mut emit = |value| {
                        let mut sort_key = key(value);
                        sort_key.copy_within(prefix..width, 0);
                        visit(sort_key, value);
                    };
                    // Every rank pass visits all finite values. Accumulate
                    // density on the first pass in the original row order.
                    if let Some((bins, domain)) = density_pass.take() {
                        scan_numbers::<T>(column, direct, finite, |value| {
                            bins.push(domain.position(value));
                            emit(value);
                        })
                    } else {
                        scan_numbers::<T>(column, direct, finite, emit)
                    }
                },
            )?
            .expect("nonempty finite column")
        };
        let mut labels = Vec::with_capacity(ranks.len());
        for (low, high, remainder) in ranks {
            labels.push(format_quantile::<T>(kind, low, high, remainder, as_float)?);
        }
        quantiles = labels.try_into().ok();
        if let Some(bins) = bins {
            let midpoint = format_quantile::<T>(kind, low, high, 50, as_float)?;
            let labels = if low.is_eq(high) {
                [String::new(), midpoint, String::new()]
            } else {
                [
                    formatted_range.0.clone(),
                    midpoint,
                    formatted_range.1.clone(),
                ]
            };
            density = Some(bins.finish(labels));
        }
        Some(formatted_range)
    } else {
        None
    };
    Ok((
        summary.nulls,
        ProfileDetails::Number {
            range,
            quantiles,
            non_finite: summary.non_finite,
            density,
        },
    ))
}

fn format_quantile<T: ArrowNumericType>(
    kind: &DataType,
    low: T::Native,
    high: T::Native,
    remainder: usize,
    as_float: Option<fn(T::Native) -> f64>,
) -> Result<String>
where
    T::Native: ArrowNativeTypeOp,
{
    if remainder == 0 || low.is_eq(high) {
        let array = native_array::<T>(vec![low], kind);
        return Ok(array_value_to_string(&array, 0)?);
    }
    if let Some(as_float) = as_float {
        let a = as_float(low);
        let b = as_float(high);
        let ratio = remainder as f64 / 100.;
        let value = if a.is_sign_negative() == b.is_sign_negative() {
            a + (b - a) * ratio
        } else {
            a * (1. - ratio) + b * ratio
        };
        return Ok(lexical_to_string(value));
    }
    let unsigned = kind.is_unsigned_integer();
    let scale = match kind {
        DataType::Decimal32(_, scale)
        | DataType::Decimal64(_, scale)
        | DataType::Decimal128(_, scale)
        | DataType::Decimal256(_, scale) => i32::from(*scale),
        _ => 0,
    };
    Ok(interpolate_decimal(
        native_integer(low, unsigned),
        native_integer(high, unsigned),
        remainder,
        scale,
    ))
}

/// The rank fraction has denominator 100. Splitting the operands avoids
/// overflow even for Decimal256 while preserving fractional output exactly.
fn interpolate_decimal(a: i256, b: i256, remainder: usize, scale: i32) -> String {
    let divisor = i256::from_i128(100);
    let right = i256::from_i128(remainder as i128);
    let left = divisor - right;
    let mut integer = (a / divisor) * left + (b / divisor) * right;
    let mut fraction = (a % divisor) * left + (b % divisor) * right;
    integer += fraction / divisor;
    fraction %= divisor;
    // Align integer and fractional signs before decimal-string formatting.
    if integer.is_negative() && fraction > i256::ZERO {
        integer += i256::ONE;
        fraction -= divisor;
    } else if integer > i256::ZERO && fraction.is_negative() {
        integer -= i256::ONE;
        fraction += divisor;
    }
    let negative = integer.is_negative() || fraction.is_negative();
    let digits = format!("{}{:02}", integer.wrapping_abs(), fraction.as_i128().abs());
    let scale = scale + 2;
    let mut display = if scale <= 0 {
        format!("{digits}{}", "0".repeat((-scale) as usize))
    } else {
        let scale = scale as usize;
        let digits = format!("{digits:0>width$}", width = scale + 1);
        let (whole, decimal) = digits.split_at(digits.len() - scale);
        let decimal = decimal.trim_end_matches('0');
        if decimal.is_empty() {
            whole.to_owned()
        } else {
            format!("{whole}.{decimal}")
        }
    };
    let first = display
        .find(|character| character != '0')
        .unwrap_or(display.len());
    if first == display.len() {
        display = "0".to_owned();
    } else if first > 0 {
        let remove = if display.as_bytes()[first] == b'.' {
            first - 1
        } else {
            first
        };
        display.drain(..remove);
    }
    if negative {
        display.insert(0, '-');
    }
    display
}

struct DensityBins {
    counts: [f64; DENSITY_POINTS],
    seen: usize,
    mean: f64,
    variance: f64,
}

impl DensityBins {
    fn new() -> Self {
        Self {
            counts: [0.; DENSITY_POINTS],
            seen: 0,
            mean: 0.,
            variance: 0.,
        }
    }

    fn push(&mut self, position: f64) {
        let x = position.clamp(0., 1.);
        let position = x * (DENSITY_POINTS - 1) as f64;
        let bin = position.floor() as usize;
        let fraction = position - bin as f64;
        self.counts[bin] += 1. - fraction;
        if bin + 1 < self.counts.len() {
            self.counts[bin + 1] += fraction;
        }
        self.seen += 1;
        let delta = x - self.mean;
        self.mean += delta / self.seen as f64;
        self.variance += delta * (x - self.mean);
    }

    fn fill_midpoint(&mut self, count: usize) {
        self.push(0.5);
        for bin in &mut self.counts {
            *bin *= count as f64;
        }
        self.seen = count;
    }

    fn finish(self, labels: [String; 3]) -> Density {
        let sigma = (self.variance.max(0.) / self.seen as f64).sqrt();
        let bandwidth =
            (1.06 * sigma * (self.seen as f64).powf(-0.2) * (DENSITY_POINTS - 1) as f64)
                .clamp(1.25, DENSITY_POINTS as f64 / 3.);
        let weights: [f64; DENSITY_POINTS] = std::array::from_fn(|distance| {
            let z = distance as f64 / bandwidth;
            (-0.5 * z * z).exp()
        });
        let mut points: Vec<f64> = (0usize..DENSITY_POINTS)
            .map(|point| {
                self.counts
                    .iter()
                    .enumerate()
                    .map(|(bin, count)| count * weights[point.abs_diff(bin)])
                    .sum()
            })
            .collect();
        let peak = points.iter().copied().fold(0., f64::max);
        for point in &mut points {
            *point /= peak;
        }
        Density { points, labels }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::{
        array::{
            ArrayRef, BooleanArray, Date32Array, Decimal256Array, Float64Array, Int32Array,
            StringArray, StringDictionaryBuilder, UInt64Array,
        },
        record_batch::RecordBatch,
    };
    use std::sync::Arc;

    fn result(arrays: impl IntoIterator<Item = (&'static str, ArrayRef)>) -> Result<QueryResult> {
        let batch = RecordBatch::try_from_iter(arrays)?;
        let columns = crate::arrow_types::columns_from_schema(&batch.schema());
        let data = ArrowTable::new(columns.clone(), vec![batch])?;
        Ok(QueryResult {
            columns,
            data,
            ..Default::default()
        })
    }

    #[test]
    fn indexed_types_count_only_current_rows() -> Result<()> {
        let mut regions = StringDictionaryBuilder::<Int8Type>::new();
        regions.append("华东")?;
        regions.append("华南")?;
        regions.append_null();
        regions.append("华东")?;
        let mut result = result([
            ("region", Arc::new(regions.finish()) as ArrayRef),
            (
                "enabled",
                Arc::new(BooleanArray::from(vec![
                    Some(true),
                    Some(true),
                    None,
                    Some(false),
                ])),
            ),
            (
                "day",
                Arc::new(Date32Array::from(vec![
                    Some(10),
                    Some(-400),
                    None,
                    Some(-2),
                ])),
            ),
            (
                "description",
                Arc::new(StringArray::from(vec![
                    Some("A multiline\nparagraph"),
                    None,
                    None,
                    Some("Second entry"),
                ])),
            ),
        ])?;
        result.data = result
            .data
            .with_row_indices(UInt64Array::from(vec![3, 2, 0]))?;
        let cancel = AtomicBool::new(false);
        let profile = |column| {
            calculate_profile(
                &result,
                column,
                ProfileMode::Auto,
                10,
                DEFAULT_CATEGORY_CHART_BARS,
                &cancel,
            )
        };
        let region = profile(0)?;
        assert_eq!((region.rows, region.nulls), (3, 1));
        assert!(
            matches!(region.details, ProfileDetails::Category { distinct: 1, common }
            if common == [("华东".into(), 2)])
        );
        let boolean = profile(1)?;
        assert_eq!(boolean.nulls, 1);
        assert!(matches!(
            boolean.details,
            ProfileDetails::Boolean {
                trues: 1,
                falses: 1
            }
        ));
        let date = profile(2)?;
        assert_eq!(date.nulls, 1);
        assert!(
            matches!(date.details, ProfileDetails::Temporal(Some((min, max)))
            if min == "1969-12-30" && max == "1970-01-11")
        );
        let text = profile(3)?;
        assert_eq!(text.nulls, 1);
        assert!(matches!(text.details, ProfileDetails::NullsOnly));
        assert!(
            calculate_profile(
                &result,
                0,
                ProfileMode::Auto,
                10,
                DEFAULT_CATEGORY_CHART_BARS,
                &AtomicBool::new(true)
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn wide_numeric_interpolation_and_chart_labels_preserve_precision() -> Result<()> {
        let first = i256::from_string("123456789012345678901234567890123456789").unwrap();
        let result = result([
            (
                "wide",
                Arc::new(UInt64Array::from(vec![u64::MAX - 1, u64::MAX])) as ArrayRef,
            ),
            (
                "decimal",
                Arc::new(
                    Decimal256Array::from(vec![first, first + i256::ONE])
                        .with_precision_and_scale(76, 2)?,
                ),
            ),
        ])?;
        let cancel = AtomicBool::new(false);
        let number = |column| {
            calculate_profile(
                &result,
                column,
                ProfileMode::Auto,
                2,
                DEFAULT_CATEGORY_CHART_BARS,
                &cancel,
            )
        };
        let ProfileDetails::Number {
            range: Some((min, max)),
            quantiles: Some(percentiles),
            density: Some(chart),
            ..
        } = number(0)?.details
        else {
            panic!("Expected numeric statistics and chart")
        };
        assert_eq!(
            (min.as_str(), max.as_str()),
            ("18446744073709551614", "18446744073709551615")
        );
        assert_eq!(percentiles[1], "18446744073709551614.5");
        assert_eq!(chart.labels[1], percentiles[1]);
        assert_eq!(chart.points.len(), DENSITY_POINTS);
        assert!(
            chart
                .points
                .iter()
                .all(|point| point.is_finite() && (0.0..=1.0).contains(point))
        );
        let ProfileDetails::Number {
            quantiles: Some(percentiles),
            ..
        } = number(1)?.details
        else {
            panic!("Expected Decimal quantiles")
        };
        assert_eq!(percentiles[1], "1234567890123456789012345678901234567.895");
        for column in 0..2 {
            let category =
                calculate_profile(&result, column, ProfileMode::Category, 2, 1, &cancel)?;
            assert!(
                matches!(category.details, ProfileDetails::Category { distinct: 2, common }
                if common.len() == 1 && common[0].1 == 1)
            );
        }
        assert_eq!(
            interpolate_decimal(i256::from_i128(-3), i256::from_i128(2), 50, 1),
            "-0.05"
        );
        assert_eq!(
            interpolate_decimal(i256::from_i128(-100), i256::from_i128(1), 99, 0),
            "-0.01"
        );
        assert_eq!(
            interpolate_decimal(i256::from_i128(-1), i256::from_i128(100), 1, -3),
            "10"
        );
        Ok(())
    }

    #[test]
    fn chart_cutoff_retains_full_numeric_summary_and_sparse_values() -> Result<()> {
        let rows = 2_000_001;
        let dense = Int32Array::from_iter_values(0..rows);
        let sparse = Float64Array::from_iter((0..rows).map(|row| match row {
            0 => Some(f64::INFINITY),
            row if row == rows - 3 => Some(10.),
            row if row == rows - 2 => Some(20.),
            row if row == rows - 1 => Some(30.),
            _ => None,
        }));
        let result = result([
            ("dense", Arc::new(dense) as ArrayRef),
            ("sparse", Arc::new(sparse)),
        ])?;
        let cancel = AtomicBool::new(false);
        let dense = calculate_profile(
            &result,
            0,
            ProfileMode::Auto,
            2_000_000,
            DEFAULT_CATEGORY_CHART_BARS,
            &cancel,
        )?;
        assert!(dense.chart_limited);
        assert_eq!(dense.rows, rows as usize);
        assert!(matches!(dense.details, ProfileDetails::Number {
            range: Some((min, max)), quantiles: Some(q), density: None, ..
        } if min == "0" && max == "2000000" && q[1] == "1000000" && q[3] == "1800000"));
        let sparse = calculate_profile(
            &result,
            1,
            ProfileMode::Auto,
            0,
            DEFAULT_CATEGORY_CHART_BARS,
            &cancel,
        )?;
        assert!(sparse.chart_limited);
        assert_eq!(sparse.nulls, rows as usize - 4);
        assert!(matches!(sparse.details, ProfileDetails::Number {
            range: Some((min, max)), quantiles: Some(q), non_finite: 1, density: None,
        } if min == "10.0" && max == "30.0" && q[1] == "20.0" && q[3] == "28.0"));
        Ok(())
    }

    #[test]
    fn category_chart_limit_does_not_change_distinct_or_frequencies() -> Result<()> {
        let values = StringArray::from_iter(
            (0..12)
                .flat_map(|index| std::iter::repeat_n(Some(format!("group-{index:02}")), index + 1))
                .chain([None]),
        );
        let result = result([("group", Arc::new(values) as ArrayRef)])?;
        let cancel = AtomicBool::new(false);
        for (row_limit, bar_limit, expected_bars) in [
            (79, 0, 0),
            (79, 3, 3),
            (79, 10, 10),
            (79, 50, 12),
            (78, 100, 0),
        ] {
            let stats =
                calculate_profile(&result, 0, ProfileMode::Auto, row_limit, bar_limit, &cancel)?;
            assert_eq!((stats.rows, stats.nulls), (79, 1));
            assert_eq!(stats.chart_limited, row_limit < stats.rows);
            let ProfileDetails::Category { distinct, common } = stats.details else {
                panic!("Expected category statistics");
            };
            assert_eq!(distinct, 12);
            assert_eq!(
                common,
                (0..expected_bars)
                    .map(|rank| (format!("group-{:02}", 11 - rank), 12 - rank))
                    .collect::<Vec<_>>()
            );
        }
        Ok(())
    }

    #[test]
    fn large_category_counts_remain_exact_with_or_without_bars() -> Result<()> {
        let count = 32_771;
        let array = StringArray::from_iter(
            (0..count)
                .map(|index| Some(format!("group-{index}")))
                .chain((0..300).map(|_| Some("group-7".into())))
                .chain([None]),
        );
        let result = result([("group", Arc::new(array) as ArrayRef)])?;
        let stats = calculate_profile(
            &result,
            0,
            ProfileMode::Category,
            usize::MAX,
            usize::MAX,
            &AtomicBool::new(false),
        )?;
        assert_eq!(stats.nulls, 1);
        assert!(
            matches!(stats.details, ProfileDetails::Category { distinct, common }
            if distinct == count && common.len() == count && common[0] == ("group-7".into(), 301))
        );
        let no_chart = calculate_profile(
            &result,
            0,
            ProfileMode::Category,
            0,
            10,
            &AtomicBool::new(false),
        )?;
        assert_eq!(no_chart.nulls, 1);
        assert!(
            matches!(no_chart.details, ProfileDetails::Category { distinct, common }
            if distinct == count && common.is_empty())
        );
        Ok(())
    }

    #[test]
    fn category_memory_cap_fails_before_inserting_or_growing() -> Result<()> {
        let mut categories = HashMap::new();
        insert_category(&mut categories, "first", (), 1)?;
        let capacity = categories.capacity();
        let error = insert_category(&mut categories, "second", (), 1).unwrap_err();
        assert_eq!(error.to_string(), CATEGORY_MEMORY_ERROR);
        assert_eq!(categories.capacity(), capacity);
        assert_eq!(categories.len(), 1);
        assert!(categories.contains_key("first"));
        Ok(())
    }
}
