//! Clipboard WHERE generation matching the original ParquetGridView workflow.
//! Values are grouped independently by column, deduplicated, and joined with
//! AND; selected row pairings are intentionally not preserved.

use std::{cmp::Ordering, collections::HashSet};

use crate::engine::{Cell, ColumnInfo, QueryResult};

/// Generate a filter accepted by the engine, including the optional WHERE
/// prefix used by this application's clipboard UI. Invalid/empty selections
/// return an empty string instead of panicking or generating a partial filter.
#[cfg(test)]
pub fn copy_as_where(result: &QueryResult, start: (usize, usize), end: (usize, usize)) -> String {
    let (first_row, last_row) = (start.0.min(end.0), start.0.max(end.0));
    let (first_col, last_col) = (start.1.min(end.1), start.1.max(end.1));
    // Validate before collecting inclusive ranges: an invalid usize::MAX end
    // must return immediately, not attempt a huge allocation or overflow.
    if last_row >= result.row_count() || last_col >= result.columns.len() {
        return String::new();
    }
    let rows = (first_row..=last_row).collect::<Vec<_>>();
    let columns = (first_col..=last_col).collect::<Vec<_>>();
    copy_as_where_indices(result, &rows, &columns)
}

/// Copy an explicit set of source row/column indices, for example after frozen
/// panes map a contiguous visual selection to noncontiguous result positions.
/// Column clauses follow the supplied order; repeated columns are emitted once.
/// Values retain the same per-column deduplication and exact sorting as the
/// rectangular selection API. No unselected row or column is inspected.
pub fn copy_as_where_indices(result: &QueryResult, rows: &[usize], columns: &[usize]) -> String {
    if rows.is_empty()
        || columns.is_empty()
        || rows.iter().any(|&row| row >= result.row_count())
        || columns.iter().any(|&column| column >= result.columns.len())
    {
        return String::new();
    }
    let mut clauses = Vec::new();
    let mut seen_columns = HashSet::new();
    for &column_index in columns {
        if !seen_columns.insert(column_index) {
            continue;
        }
        let column = &result.columns[column_index];
        let expression = column_expression(column);
        let mut has_null = false;
        let mut values = Vec::new();
        let mut seen = HashSet::new();
        for &row_index in rows {
            if result.is_null(row_index, column_index) {
                has_null = true;
            } else {
                let cell = result.cell(row_index, column_index);
                let Some(literal) = value_literal(column, &cell) else {
                    return String::new();
                };
                if seen.insert(literal.clone()) {
                    values.push((cell.text, literal));
                }
            }
        }
        values.sort_by(|(left, _), (right, _)| {
            if is_numeric(&column.data_type) {
                numeric_order(left, right)
            } else {
                left.cmp(right)
            }
        });
        let condition = if values.is_empty() {
            format!("{expression} IS NULL")
        } else {
            let comparison = if values.len() == 1 && !has_null {
                format!("{expression} = {}", values[0].1)
            } else {
                format!(
                    "{expression} IN ({})",
                    values
                        .into_iter()
                        .map(|(_, literal)| literal)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            if has_null {
                format!("({comparison} OR {expression} IS NULL)")
            } else {
                comparison
            }
        };
        clauses.push(condition);
    }
    if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    }
}

fn identifier(name: &str) -> String {
    // The engine accepts both doubled ] and DataView-style backslash escapes.
    // Escape backslashes first so a source name containing \] is not consumed
    // as an escape for the closing delimiter.
    format!("[{}]", name.replace('\\', "\\\\").replace(']', "]]"))
}

fn nested(data_type: &str) -> bool {
    data_type.starts_with("STRUCT(")
        || data_type.starts_with("MAP(")
        || data_type.starts_with("UNION(")
        || data_type.contains('[')
}

fn column_expression(column: &ColumnInfo) -> String {
    let name = identifier(&column.name);
    if column.data_type == "BLOB" {
        format!("hex({name})")
    } else if nested(&column.data_type) {
        // Cell.text for complex types comes from the engine's to_json display
        // projection, not a different human-readable STRUCT notation.
        format!("CAST(to_json({name}) AS VARCHAR)")
    } else {
        name
    }
}

fn quoted(value: &str) -> String {
    if value.contains('\0') {
        // Represent a NUL without embedding it directly in an expression string.
        format!("decode(from_hex('{}'))", hex(value.as_bytes()))
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn is_numeric(data_type: &str) -> bool {
    matches!(
        data_type,
        "TINYINT"
            | "SMALLINT"
            | "INTEGER"
            | "BIGINT"
            | "INT64"
            | "HUGEINT"
            | "UTINYINT"
            | "USMALLINT"
            | "UINTEGER"
            | "UBIGINT"
            | "UHUGEINT"
            | "FLOAT"
            | "REAL"
            | "DOUBLE"
            | "BIGNUM"
            | "VARINT"
    ) || decimal_type(data_type)
}

fn decimal_type(data_type: &str) -> bool {
    data_type
        .strip_prefix("DECIMAL(")
        .and_then(|text| text.strip_suffix(')'))
        .is_some_and(|parameters| {
            parameters.split_once(',').is_some_and(|(precision, scale)| {
                let precision = precision.trim().parse::<u8>().ok();
                let scale = scale.trim().parse::<u8>().ok();
                matches!((precision, scale), (Some(p), Some(s)) if (1..=38).contains(&p) && s <= p)
            })
        })
}

fn value_literal(column: &ColumnInfo, cell: &Cell) -> Option<String> {
    let value = quoted(&cell.text);
    if column.data_type == "BLOB" {
        return Some(quoted(&hex(cell.bytes.as_ref()?)));
    }
    if is_numeric(&column.data_type)
        || matches!(
            column.data_type.as_str(),
            "BOOLEAN"
                | "DATE"
                | "TIME"
                | "TIME WITH TIME ZONE"
                | "TIMESTAMP"
                | "TIMESTAMP_S"
                | "TIMESTAMP_MS"
                | "TIMESTAMP_NS"
                | "TIMESTAMP WITH TIME ZONE"
                | "UUID"
                | "INTERVAL"
                | "BIT"
        )
    {
        // Preserve DECIMAL(38,...), unsigned/128-bit integers, timestamps and
        // non-finite floats without going through a lossy Rust f64 conversion.
        let sql_type = if column.data_type == "INT64" {
            "BIGINT"
        } else {
            &column.data_type
        };
        Some(format!("CAST({value} AS {sql_type})"))
    } else {
        Some(value)
    }
}

/// Compare decimal/scientific strings exactly for deterministic numeric IN
/// ordering, without rounding large integers or high-precision decimals.
fn numeric_order(left: &str, right: &str) -> Ordering {
    fn finite(value: &str) -> Option<(bool, String, i64)> {
        let (negative, value) = if let Some(value) = value.strip_prefix('-') {
            (true, value)
        } else {
            (false, value.strip_prefix('+').unwrap_or(value))
        };
        let (mantissa, exponent) = value.split_once(['e', 'E']).unwrap_or((value, "0"));
        let exponent = exponent.parse::<i32>().ok()? as i64;
        let fractional = mantissa
            .split_once('.')
            .map(|(_, part)| part.len())
            .unwrap_or(0);
        let digits = mantissa.replace('.', "");
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            return Some((false, "0".into(), 0));
        }
        Some((negative, digits.into(), exponent - fractional as i64))
    }
    let (
        Some((left_negative, left_digits, left_power)),
        Some((right_negative, right_digits, right_power)),
    ) = (finite(left), finite(right))
    else {
        return left.cmp(right);
    };
    if left_negative != right_negative {
        return if left_negative {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let order = if left_digits == "0" || right_digits == "0" {
        (left_digits != "0").cmp(&(right_digits != "0"))
    } else {
        let magnitude =
            (left_digits.len() as i64 + left_power).cmp(&(right_digits.len() as i64 + right_power));
        if magnitude == Ordering::Equal {
            let length = left_digits.len().max(right_digits.len());
            left_digits
                .bytes()
                .chain(std::iter::repeat(b'0'))
                .take(length)
                .cmp(
                    right_digits
                        .bytes()
                        .chain(std::iter::repeat(b'0'))
                        .take(length),
                )
        } else {
            magnitude
        }
    };
    if left_negative {
        order.reverse()
    } else {
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_values_keep_exact_binary_nested_decimal_and_column_identity() {
        let columns = vec![
            ColumnInfo {
                name: "blob\\] name".into(),
                data_type: "BLOB".into(),
                nullable: true,
            },
            ColumnInfo {
                name: "nested".into(),
                data_type: "STRUCT(label VARCHAR, items BIGINT[])".into(),
                nullable: true,
            },
            ColumnInfo {
                name: "amount".into(),
                data_type: "DECIMAL(38,18)".into(),
                nullable: true,
            },
            ColumnInfo {
                name: "ignored".into(),
                data_type: "VARCHAR".into(),
                nullable: true,
            },
        ];
        let value = || {
            vec![
                Cell {
                    bytes: Some(vec![0, 255, 39]),
                    ..Default::default()
                },
                Cell {
                    json: Some(serde_json::json!({"label":"O'Brien","items":[1,null,2]})),
                    ..Default::default()
                },
                Cell {
                    text: "-12345678901234567890.123456789012345678".into(),
                    ..Default::default()
                },
                Cell {
                    text: "unselected".into(),
                    ..Default::default()
                },
            ]
        };
        let result = QueryResult::from_cells(columns, vec![value(), value()]);
        let filter = copy_as_where_indices(&result, &[1, 0, 1], &[2, 0, 1, 2]);
        assert_eq!(filter.matches(" AND ").count(), 2);
        assert!(
            filter.contains("CAST('-12345678901234567890.123456789012345678' AS DECIMAL(38,18))")
        );
        assert!(filter.contains("00FF27"));
        assert!(filter.contains("O''Brien"));
        assert!(!filter.contains("unselected"));
        assert!(filter.find("[amount]").unwrap() < filter.find("hex(").unwrap());
        assert_eq!(numeric_order("1.1e-20", "1e-20"), Ordering::Greater);
        assert_eq!(numeric_order("-0", "0.000"), Ordering::Equal);
    }

    #[test]
    fn selection_sets_do_not_turn_into_selected_row_pairings() {
        let result = QueryResult::from_cells(
            vec![
                ColumnInfo {
                    name: "left] name".into(),
                    data_type: "BIGINT".into(),
                    nullable: true,
                },
                ColumnInfo {
                    name: "right text".into(),
                    data_type: "VARCHAR".into(),
                    nullable: true,
                },
            ],
            vec![
                vec![
                    Cell {
                        text: "1".into(),
                        ..Default::default()
                    },
                    Cell {
                        text: "O'Brien".into(),
                        ..Default::default()
                    },
                ],
                vec![
                    Cell {
                        text: "2".into(),
                        ..Default::default()
                    },
                    Cell {
                        text: "上海".into(),
                        ..Default::default()
                    },
                ],
                vec![
                    Cell {
                        text: "1".into(),
                        ..Default::default()
                    },
                    Cell {
                        is_null: true,
                        ..Default::default()
                    },
                ],
            ],
        );
        let filter = copy_as_where_indices(&result, &[0, 1, 2], &[0, 1]);
        assert!(filter.contains("[left]] name] IN (CAST('1' AS BIGINT), CAST('2' AS BIGINT))"));
        assert!(filter.contains("[right text] IN ('O''Brien', '上海') OR [right text] IS NULL"));
        assert_eq!(filter.matches(" AND ").count(), 1);
    }

    #[test]
    fn explicit_indices_reject_empty_or_invalid_inputs_and_rectangle_api_forwards_safely() {
        let result = QueryResult::from_cells(
            vec![
                ColumnInfo {
                    name: "id".into(),
                    data_type: "BIGINT".into(),
                    nullable: false,
                },
                ColumnInfo {
                    name: "text".into(),
                    data_type: "VARCHAR".into(),
                    nullable: true,
                },
            ],
            vec![
                vec![
                    Cell {
                        text: "9007199254740993".into(),
                        ..Default::default()
                    },
                    Cell {
                        text: "present".into(),
                        ..Default::default()
                    },
                ],
                vec![
                    Cell {
                        text: "2".into(),
                        ..Default::default()
                    },
                    Cell {
                        is_null: true,
                        ..Default::default()
                    },
                ],
            ],
        );
        // Unselected values cannot affect a valid selection.
        assert_eq!(
            copy_as_where_indices(&result, &[0], &[0]),
            "WHERE [id] = CAST('9007199254740993' AS BIGINT)"
        );
        assert_eq!(
            copy_as_where_indices(&result, &[1], &[1]),
            "WHERE [text] IS NULL"
        );
        for (rows, columns) in [
            (vec![], vec![0]),
            (vec![0], vec![]),
            (vec![usize::MAX], vec![0]),
            (vec![0], vec![usize::MAX]),
            (vec![2], vec![0]),
            (vec![0], vec![2]),
        ] {
            assert!(copy_as_where_indices(&result, &rows, &columns).is_empty());
        }
        assert!(copy_as_where(&result, (0, 0), (usize::MAX, 0)).is_empty());
        assert!(copy_as_where(&result, (0, 0), (0, usize::MAX)).is_empty());
        assert!(
            copy_as_where(&QueryResult::default(), (0, 0), (usize::MAX, usize::MAX)).is_empty()
        );
        assert_eq!(
            copy_as_where(&result, (1, 0), (0, 0)),
            copy_as_where_indices(&result, &[0, 1], &[0])
        );
        assert_eq!(
            copy_as_where_indices(&result, &[0, 1, 0], &[0, 0]),
            "WHERE [id] IN (CAST('2' AS BIGINT), CAST('9007199254740993' AS BIGINT))"
        );
        assert_eq!(
            copy_as_where_indices(&result, &[0, 1], &[1]),
            "WHERE ([text] IN ('present') OR [text] IS NULL)"
        );
    }

    #[test]
    fn typed_snapshot_rejects_ragged_rows_instead_of_treating_absent_cells_as_null() {
        // Arrow snapshots have a rectangular schema. The old row-vector
        // fixture's absent cell must fail at construction, rather than become
        // a valid SQL NULL that changes clipboard filter semantics.
        let malformed = std::panic::catch_unwind(|| {
            QueryResult::from_cells(
                vec![
                    ColumnInfo {
                        name: "id".into(),
                        data_type: "BIGINT".into(),
                        nullable: false,
                    },
                    ColumnInfo {
                        name: "text".into(),
                        data_type: "VARCHAR".into(),
                        nullable: true,
                    },
                ],
                vec![vec![Cell {
                    text: "9007199254740993".into(),
                    ..Default::default()
                }]],
            )
        });
        assert!(malformed.is_err());
    }
}
