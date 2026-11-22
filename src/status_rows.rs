//! Describe the published table, its source window and any restrictions on its rows.
use crate::{
    engine::{QueryRequest, QueryResult},
    formatting::count_label,
    i18n::t,
};

pub(super) fn row_summary(
    result: &QueryResult,
    request: &QueryRequest,
    parent_rows: Option<usize>,
) -> (String, String) {
    let shown = result.row_count();
    let loaded = result.loaded_rows;
    let filtered = !request.filter.trim().is_empty();
    let capped = request.top_n.is_some_and(|limit| limit < loaded);
    let whole_range = request.offset == 0 && result.total_rows_known && loaded == result.total_rows;
    let mut details = Vec::new();
    let mut range = None;
    if !request.whole_source && loaded > 0 && !whole_range {
        let label = if result.total_rows_known {
            t("Rows {first}–{last} of {total}").replace("{total}", &count_label(result.total_rows))
        } else {
            t("Rows {first}–{last}")
        }
        .replace("{first}", &count_label(request.offset.saturating_add(1)))
        .replace(
            "{last}",
            &count_label(request.offset.saturating_add(loaded)),
        );
        if filtered || capped || parent_rows.is_some() {
            details.push(format!("{} · {label}", t("Loaded range")));
        } else {
            details.push(format!("{} · {}", t("Loaded rows"), count_label(loaded)));
        }
        range = Some(label);
    } else if result.total_rows_known {
        details.push(format!(
            "{} · {}",
            t("Total source rows"),
            count_label(result.total_rows)
        ));
    }
    if !result.total_rows_known {
        details.push(t(
            "Total rows will be available after the text row index reaches the end of the file.",
        ));
    }
    if !request.whole_source && loaded == 0 && !whole_range {
        details.push(t("No rows loaded"));
    }

    // A bounded range query can stop after Top N without counting all matches.
    // Whole-source filtering always counts the matches before the sort/limit.
    let matches = if request.whole_source {
        Some(loaded)
    } else if filtered {
        result.filtered_rows.or_else(|| (!capped).then_some(loaded))
    } else {
        None
    };
    if let Some(matches) = matches {
        let label = if request.whole_source {
            t("Matches in entire file")
        } else {
            t("Matches in loaded range")
        };
        details.push(format!("{label} · {}", count_label(matches)));
    } else if filtered {
        details.push(t("Filtered within loaded range"));
    }
    if let Some(limit) = request.top_n {
        details.push(format!("{} · {}", t("Top N"), count_label(limit)));
    }

    let label = if let Some(parent) = parent_rows {
        details.insert(
            0,
            format!("{} · {}", t("Rows in parent result"), count_label(parent)),
        );
        if shown == 1 {
            t("1 group row")
        } else {
            t("{count} group rows").replace("{count}", &count_label(shown))
        }
    } else if filtered || request.whole_source || capped {
        if shown == 1 {
            t("1 row shown")
        } else {
            t("{count} rows shown").replace("{count}", &count_label(shown))
        }
    } else if let Some(range) = range {
        range
    } else if shown == 1 {
        t("1 row")
    } else {
        t("{count} rows").replace("{count}", &count_label(shown))
    };
    (label, details.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columnar::ArrowTable;
    use arrow::{
        array::{ArrayRef, Int64Array},
        record_batch::RecordBatch,
    };
    use std::sync::Arc;

    fn result(visible: usize, loaded: usize, total: usize, known: bool) -> QueryResult {
        let array: ArrayRef = Arc::new(Int64Array::from_iter_values(0..visible as i64));
        let batch = RecordBatch::try_from_iter([("id", array)]).unwrap();
        let columns = crate::arrow_types::columns_from_schema(&batch.schema());
        QueryResult {
            data: ArrowTable::new(columns, vec![batch]).unwrap(),
            loaded_rows: loaded,
            total_rows: total,
            total_rows_known: known,
            ..Default::default()
        }
    }

    #[test]
    fn source_range_and_unknown_csv_prefix() {
        let req = QueryRequest {
            offset: 1000,
            ..Default::default()
        };
        let (label, tip) = row_summary(&result(12, 12, 5000, true), &req, None);
        assert_eq!(label, "Rows 1,001–1,012 of 5,000");
        assert_eq!(tip, "Loaded rows · 12");
        let (label, tip) = row_summary(&result(12, 12, 1234, false), &req, None);
        assert_eq!(label, "Rows 1,001–1,012");
        assert!(!tip.contains("1,234"));
        assert!(tip.contains("end of the file"));
        let (label, tip) = row_summary(&result(0, 0, 7, true), &req, None);
        assert_eq!(label, "0 rows");
        assert!(tip.contains("No rows loaded"));
        assert_eq!(
            row_summary(&result(1, 1, 1, true), &QueryRequest::default(), None).0,
            "1 row"
        );
    }

    #[test]
    fn hidden_filter_counts_and_whole_source_matches() {
        let mut req = QueryRequest {
            filter: "id > 0".into(),
            top_n: Some(4),
            ..Default::default()
        };
        let mut data = result(4, 10, 100, true);
        let (label, tip) = row_summary(&data, &req, None);
        assert_eq!(label, "4 rows shown");
        assert!(tip.contains("Top N · 4"));
        assert!(tip.contains("Filtered within loaded range"));
        assert!(!tip.contains("Matches in loaded range"));
        req.whole_source = true;
        data.filtered_rows = Some(10);
        let (label, tip) = row_summary(&data, &req, None);
        assert_eq!(label, "4 rows shown");
        assert!(tip.contains("Matches in entire file · 10"));
        assert!(tip.contains("Total source rows · 100"));
        req.whole_source = false;
        req.top_n = None;
        let mut empty = result(0, 10, 10, true);
        empty.filtered_rows = Some(0);
        let (label, tip) = row_summary(&empty, &req, None);
        assert_eq!(label, "0 rows shown");
        assert!(tip.contains("Matches in loaded range · 0"));
    }

    #[test]
    fn drilldown_uses_group_rows_and_parent_cap() {
        let req = QueryRequest {
            filter: "id > 0".into(),
            top_n: Some(5),
            whole_source: true,
            ..Default::default()
        };
        let mut data = result(2, 12, 100, true);
        data.filtered_rows = Some(12);
        let (label, tip) = row_summary(&data, &req, Some(5));
        assert_eq!(label, "2 group rows");
        assert!(tip.contains("Rows in parent result · 5"));
        assert!(tip.contains("Matches in entire file · 12"));
        let (label, tip) =
            row_summary(&result(1, 12, 12, true), &QueryRequest::default(), Some(12));
        assert_eq!(label, "1 group row");
        assert!(tip.contains("Rows in parent result · 12"));
    }
}
