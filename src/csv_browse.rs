//! The delimited table addresses logical source rows while its Arrow pages remain bounded.
use super::*;
use crate::columnar::ArrowTable;
use crate::engine::csv_source::CsvBrowse as Reader;
use arrow::array::StringViewArray;
use std::collections::VecDeque;
use std::ops::RangeInclusive;

const PAGE_ROWS: usize = 256;
const PAGE_BYTES: usize = 1024 * 1024;
const CACHE_BYTES: usize = 24 * PAGE_BYTES;
const CACHE_PAGES: usize = 96;
const CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;
const CLIPBOARD_LIMIT: &str =
    "The selection is too large for the clipboard. Select fewer cells or export a range.";

struct Page {
    start: usize,
    data: ArrowTable,
}

impl Page {
    fn local_row(&self, row: usize) -> Option<usize> {
        row.checked_sub(self.start)
            .filter(|&index| index < self.data.row_count())
    }
}

#[derive(Default)]
struct PageCache {
    pages: VecDeque<Page>,
    bytes: usize,
}

impl PageCache {
    fn insert(&mut self, page: Page) {
        let bytes = page.data.memory_size();
        if bytes > CACHE_BYTES {
            return;
        }
        while self.pages.len() >= CACHE_PAGES || self.bytes.saturating_add(bytes) > CACHE_BYTES {
            let Some(old) = self.pages.pop_back() else {
                break;
            };
            self.bytes -= old.data.memory_size();
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.pages.push_front(page);
    }

    fn lookup(&mut self, row: usize) -> Option<(ArrowTable, usize)> {
        let index = self
            .pages
            .iter()
            .position(|page| page.local_row(row).is_some())?;
        if index > 0 {
            let page = self.pages.remove(index)?;
            self.pages.push_front(page);
        }
        let page = self.pages.front()?;
        Some((page.data.clone(), row - page.start))
    }
}

/// Current and previous layout passes only. Retaining the visible strings apart
/// from the read-ahead pages avoids an eviction/refetch loop whenever the
/// visible rows occupy more pages or Arrow bytes than the LRU admits.
#[derive(Clone)]
pub(super) struct ScreenRow {
    fields: Arc<[usize]>,
    values: Arc<[Option<SharedString>]>,
}

impl ScreenRow {
    pub(super) fn cell(&self, source_column: usize) -> Option<(bool, SharedString)> {
        let index = self.fields.binary_search(&source_column).ok()?;
        Some(match self.values.get(index)? {
            Some(value) => (false, value.clone()),
            None => (true, "NULL".into()),
        })
    }
}

#[derive(Default)]
struct Viewport {
    fields: Arc<[usize]>,
    rows: HashMap<usize, (bool, ScreenRow)>,
}

impl Viewport {
    fn begin(&mut self, mut fields: Vec<usize>) -> bool {
        fields.sort_unstable();
        if self.fields.as_ref() != fields {
            self.fields = fields.into();
            self.rows.clear();
            return true;
        }
        // Rendering either the frozen or main pane renews a row. Rows that
        // leave both panes are discarded on the next pass regardless of length.
        self.rows.retain(|_, (seen, _)| std::mem::take(seen));
        false
    }

    fn lookup(&mut self, source: usize) -> Option<ScreenRow> {
        let (seen, row) = self.rows.get_mut(&source)?;
        *seen = true;
        Some(row.clone())
    }

    fn publish(
        &mut self,
        source: usize,
        data: &ArrowTable,
        local: usize,
        all_fields: bool,
    ) -> ScreenRow {
        let values = self
            .fields
            .iter()
            .enumerate()
            .map(|(projected, &physical)| {
                let col = if all_fields { physical } else { projected };
                (!data.is_null(local, col)).then(|| {
                    let text = data.display_text(local, col);
                    SharedString::from(text.replace('\n', " ↵ ").replace('\r', ""))
                })
            })
            .collect::<Vec<_>>();
        let row = ScreenRow {
            fields: self.fields.clone(),
            values: values.into(),
        };
        self.rows.insert(source, (true, row.clone()));
        row
    }

    fn failed(&mut self, source: usize) {
        let row = ScreenRow {
            fields: self.fields.clone(),
            values: Arc::default(),
        };
        self.rows.insert(source, (true, row));
    }
}

pub(super) struct CsvBrowse {
    reader: Reader,
    pub(super) pending_jump: Option<std::num::NonZeroUsize>,
    rows: usize,
    cache: PageCache,
    reading: bool,
    page_rows: usize,
    viewport: Viewport,
    cancel: Arc<AtomicBool>,
    copy_cancel: Option<Arc<AtomicBool>>,
}

impl Drop for CsvBrowse {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(cancel) = &self.copy_cancel {
            cancel.store(true, Ordering::Release);
        }
    }
}

impl CsvBrowse {
    pub(super) fn new(reader: Reader, first: &QueryResult) -> Self {
        let rows = first.row_count().max(reader.progress().indexed_rows);
        Self {
            reader,
            pending_jump: None,
            rows,
            cache: PageCache::default(),
            reading: false,
            page_rows: PAGE_ROWS,
            viewport: Viewport::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            copy_cancel: None,
        }
    }

    pub(super) fn begin_viewport(&mut self, fields: Vec<usize>) {
        if self.viewport.begin(fields) {
            self.cache = PageCache::default();
        }
    }
}

impl Viewer {
    pub(super) fn table_row_count(&self) -> usize {
        self.csv_browse.as_ref().map_or_else(
            || self.result.as_ref().map_or(0, |result| result.row_count()),
            |browse| browse.rows,
        )
    }

    pub(super) fn sync_csv_browse_rows(&mut self, indexed: usize, cx: &mut Context<Self>) {
        let Some(browse) = self.csv_browse.as_mut() else {
            return;
        };
        if indexed <= browse.rows {
            return;
        }
        browse.rows = indexed;
        self.view_rows = self.table_state.row_order(indexed);
        let count = indexed - self.frozen_row_count;
        for list in [&self.scroll.main, &self.scroll.frozen] {
            let old = list.item_count();
            let anchor = list.logical_scroll_top();
            if count > old {
                list.splice(old..old, count - old);
                list.scroll_to(anchor);
            }
        }
        cx.notify();
    }

    pub(super) fn csv_display_row(
        &mut self,
        row: usize,
        cx: &mut Context<Self>,
    ) -> Option<ScreenRow> {
        let browse = self.csv_browse.as_mut()?;
        if let Some(display) = browse.viewport.lookup(row) {
            return Some(display);
        }
        if let Some(first) = self.result.as_ref().filter(|first| row < first.row_count()) {
            return Some(browse.viewport.publish(row, &first.data, row, true));
        }
        if let Some((page, local)) = browse.cache.lookup(row) {
            return Some(browse.viewport.publish(row, &page, local, false));
        }
        if row >= browse.rows || browse.reading || browse.viewport.fields.is_empty() {
            return None;
        }
        browse.reading = true;
        let reader = browse.reader.clone();
        let cancel = browse.cancel.clone();
        let resources = self.resources.clone();
        let task_cancel = cancel.clone();
        // Align reads so scrolling upward reuses entire pages too. The previous
        // read's actual length accounts for long records and wide schemas.
        let aligned = row - row % browse.page_rows;
        let fields = browse.viewport.fields.clone();
        let requested_fields = fields.clone();
        let task = cx.background_executor().spawn(async move {
            let data = reader.read_projected_page(
                aligned,
                PAGE_ROWS,
                PAGE_BYTES,
                &requested_fields,
                &resources,
                &task_cancel,
            );
            let (start, data) = match data {
                Ok(data) if row - aligned < data.row_count() => (aligned, data),
                _ if task_cancel.load(Ordering::Acquire) => anyhow::bail!("Operation cancelled"),
                // A byte cutoff or conversion error elsewhere in this page
                // must never hide a valid row at the requested position.
                _ => (
                    row,
                    reader.read_projected_page(
                        row,
                        1,
                        PAGE_BYTES,
                        &requested_fields,
                        &resources,
                        &task_cancel,
                    )?,
                ),
            };
            anyhow::ensure!(
                !data.is_empty(),
                "The text decoder returned an incomplete row range."
            );
            Ok::<_, anyhow::Error>(Page { start, data })
        });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                let Some(browse) = this
                    .csv_browse
                    .as_mut()
                    .filter(|b| Arc::ptr_eq(&b.cancel, &cancel))
                else {
                    return;
                };
                browse.reading = false;
                if browse.viewport.fields != fields {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(page) => {
                        browse.page_rows = page.data.row_count().clamp(1, PAGE_ROWS);
                        browse
                            .viewport
                            .publish(row, &page.data, row - page.start, false);
                        browse.cache.insert(page);
                    }
                    Err(error) => {
                        browse.viewport.failed(row);
                        this.error = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        None
    }

    pub(super) fn inspect_csv_browse_selection(&mut self, cx: &mut Context<Self>) {
        let (Some(browse), Some((_, (row, column)))) = (&self.csv_browse, self.selection) else {
            return;
        };
        let reader = browse.reader.clone();
        let row = self.source_row(row);
        let column = self.source_column(column);
        let resources = self.resources.clone();
        let lifetime = browse.cancel.clone();
        self.close_preview(cx);
        let request = self.preview_request;
        let cancel = Arc::new(AtomicBool::new(false));
        self.preview_cancel = Some(cancel.clone());
        let task = cx.background_executor().spawn(async move {
            let data =
                reader.read_projected_page(row, 1, PAGE_BYTES, &[column], &resources, &cancel)?;
            anyhow::ensure!(
                !data.is_empty(),
                "The text decoder returned an incomplete row range."
            );
            Ok::<_, anyhow::Error>(data.cell(0, 0))
        });
        cx.spawn(async move |weak, cx| {
            let cell = task.await;
            let _ = weak.update(cx, |this, cx| {
                if lifetime.load(Ordering::Acquire) || this.preview_request != request {
                    return;
                }
                match cell {
                    Ok(cell) => {
                        this.close_statistics_sidebar(cx);
                        this.close_fields_sidebar();
                        this.filter_sidebar_open = false;
                        this.open_prepared_peek(
                            row,
                            column,
                            false,
                            PreparedPeek {
                                cell,
                                nested: None,
                                preview: None,
                                image: None,
                            },
                            cx,
                        );
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn copy_csv_browse_selection(&mut self, headers: bool, cx: &mut Context<Self>) {
        self.selection_menu = None;
        self.axis_menu = None;
        let Some((start, end)) = self.selection else {
            return;
        };
        let rows = start.0.min(end.0)..=start.0.max(end.0);
        let Some(result) = &self.result else { return };
        let columns = self
            .selected_source_columns()
            .into_iter()
            .map(|index| {
                let column = &result.columns[index];
                let style = self.formats.get(&column.name).cloned().unwrap_or_default();
                (index, column.clone(), style.float)
            })
            .collect::<Vec<_>>();
        let Some(browse) = self.csv_browse.as_mut() else {
            return;
        };
        if columns.is_empty() || *rows.end() >= browse.rows {
            return;
        }
        let order = self.view_rows.clone();
        let reader = browse.reader.clone();
        let resources = self.resources.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(previous) = browse.copy_cancel.replace(cancel.clone()) {
            previous.store(true, Ordering::Release);
        }
        let task_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            copy_rows(
                &reader,
                &resources,
                &task_cancel,
                &order,
                rows,
                &columns,
                headers,
            )
        });
        cx.spawn(async move |weak, cx| {
            let output = task.await;
            let _ = weak.update(cx, |this, cx| {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                if let Some(browse) = &mut this.csv_browse {
                    browse.copy_cancel = None;
                }
                match output {
                    Ok(text) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        this.notice = "Copied to clipboard".into();
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn append_tsv(out: &mut String, value: &str, separator: &str) -> anyhow::Result<()> {
    let quoted = value.contains(['\t', '\n', '\r', '"']);
    let escaped_bytes = if quoted {
        2 + value.matches('"').count()
    } else {
        0
    };
    anyhow::ensure!(
        out.len()
            .saturating_add(separator.len())
            .saturating_add(value.len())
            .saturating_add(escaped_bytes)
            <= CLIPBOARD_BYTES,
        CLIPBOARD_LIMIT
    );
    out.push_str(separator);
    if quoted {
        out.push('"');
        for (i, part) in value.split('"').enumerate() {
            if i > 0 {
                out.push_str("\"\"");
            }
            out.push_str(part);
        }
        out.push('"');
    } else {
        out.push_str(value);
    }
    Ok(())
}

fn copy_rows(
    reader: &Reader,
    resources: &crate::resource_budget::Client,
    cancel: &AtomicBool,
    order: &crate::table_state::RowOrder,
    rows: RangeInclusive<usize>,
    columns: &[(usize, ColumnInfo, FloatDisplay)],
    headers: bool,
) -> anyhow::Result<String> {
    let count = rows.end() - rows.start() + 1;
    anyhow::ensure!(
        count.saturating_mul(columns.len()).saturating_sub(1) <= CLIPBOARD_BYTES,
        CLIPBOARD_LIMIT
    );
    let mut out = String::new();
    if headers {
        for (i, (_, info, _)) in columns.iter().enumerate() {
            append_tsv(&mut out, &info.name, if i == 0 { "" } else { "\t" })?;
        }
    }
    let fields: Vec<_> = columns.iter().map(|(index, _, _)| *index).collect();
    let mut page: Option<Page> = None;
    let first = *rows.start();
    for view in rows {
        anyhow::ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
        let source = order
            .source(view)
            .ok_or_else(|| anyhow::anyhow!("The text decoder returned an incomplete row range."))?;
        if page
            .as_ref()
            .is_none_or(|page| page.local_row(source).is_none())
        {
            let data = reader.read_projected_page(
                source,
                PAGE_ROWS.min(count - (view - first)),
                PAGE_BYTES,
                &fields,
                resources,
                cancel,
            )?;
            anyhow::ensure!(
                !data.is_empty(),
                "The text decoder returned an incomplete row range."
            );
            page = Some(Page {
                start: source,
                data,
            });
        }
        let page = page.as_ref().unwrap();
        let row = source - page.start;
        for (i, (_, info, float)) in columns.iter().enumerate() {
            let sep = if i > 0 {
                "\t"
            } else if headers || view > first {
                "\n"
            } else {
                ""
            };
            if page.data.is_null(row, i) {
                append_tsv(&mut out, "", sep)?;
            } else if let Some((strings, index)) =
                page.data.array(row, i).and_then(|(array, index)| {
                    array
                        .as_any()
                        .downcast_ref::<StringViewArray>()
                        .map(|strings| (strings, index))
                })
            {
                append_tsv(&mut out, strings.value(index), sep)?;
            } else {
                let text = page.data.cell(row, i).text;
                let formatted = float.apply(&text, &info.data_type);
                append_tsv(&mut out, formatted.as_deref().unwrap_or(&text), sep)?;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_multibyte_text_is_compacted_only_for_display() -> anyhow::Result<()> {
        let original = "数据".repeat(100_000);
        let strings = StringViewArray::from(vec![Some(original.as_str()), None]);
        let batch = arrow::record_batch::RecordBatch::try_from_iter([(
            "name",
            Arc::new(strings) as arrow::array::ArrayRef,
        )])?;
        let table = ArrowTable::new(
            crate::arrow_types::columns_from_schema(&batch.schema()),
            vec![batch],
        )?;
        let table = table.with_source_row_start(501);
        let mut viewport = Viewport::default();
        viewport.begin(vec![0]);
        let value = viewport.publish(500, &table, 0, true).cell(0).unwrap();
        assert_eq!(value.1.as_ref(), "数据".repeat(1000));
        assert!(!value.0);
        assert!(viewport.publish(501, &table, 1, true).cell(0).unwrap().0);
        assert_eq!(table.source_row_number(1), Some(502));
        assert_eq!(table.cell(0, 0).text, original);
        Ok(())
    }

    #[test]
    fn wide_rows_remain_displayed_after_projected_pages_are_evicted() -> anyhow::Result<()> {
        // The physical record is over 1 MiB. A byte-bounded reader can return
        // just this one row in each page. A tall viewport also exceeds the LRU
        // page-count limit, independently of how much projection reduces bytes.
        const VISIBLE_ROWS: usize = 128;
        let physical = arrow::record_batch::RecordBatch::try_from_iter((0..128).map(|col| {
            let name = format!("field{col:03}");
            let value = format!("{col:03}{}", "x".repeat(8192 - 3));
            (
                name,
                Arc::new(StringViewArray::from(vec![value])) as arrow::array::ArrayRef,
            )
        }))?;
        let fields: Vec<_> = (60..68).collect();
        let batch = physical.project(&fields)?;
        let projection = ArrowTable::new(
            crate::arrow_types::columns_from_schema(&batch.schema()),
            vec![batch],
        )?;
        let mut viewport = Viewport::default();
        let mut cache = PageCache::default();
        let mut fetched = 0;
        let mut warm_misses = 0;
        // Each simulated background completion produces one fresh GUI frame.
        for frame in 0..VISIBLE_ROWS + 4 {
            viewport.begin(fields.clone());
            let mut requested = None;
            for row in 0..VISIBLE_ROWS {
                if viewport.lookup(row).is_some() {
                    continue;
                }
                if let Some((data, local)) = cache.lookup(row) {
                    viewport.publish(row, &data, local, false);
                } else if requested.is_none() {
                    requested = Some(row);
                }
            }
            if let Some(row) = requested {
                if frame >= VISIBLE_ROWS {
                    warm_misses += 1;
                }
                fetched += 1;
                viewport.publish(row, &projection, 0, false);
                cache.insert(Page {
                    start: row,
                    data: projection.clone(),
                });
            }
        }
        assert_eq!(fetched, VISIBLE_ROWS);
        assert_eq!(warm_misses, 0);
        assert!(cache.lookup(0).is_none());
        let displayed_bytes: usize = viewport
            .rows
            .values()
            .flat_map(|(_, r)| r.values.iter().flatten())
            .map(|v| v.len())
            .sum();
        assert_eq!(displayed_bytes, VISIBLE_ROWS * fields.len() * 2_000);
        let displayed = viewport.lookup(0).unwrap().cell(63).unwrap().1;
        assert!(displayed.starts_with("063"));
        assert_eq!(displayed.len(), 2_000);
        eprintln!(
            "128 wide rows, 128 physical fields, 8 visible: physical single-row Arrow bytes {}, projected bytes {}, visible string bytes {}, resident page bytes {}, warm rereads {}",
            physical.get_array_memory_size(),
            projection.memory_size(),
            displayed_bytes,
            cache.bytes,
            warm_misses
        );
        // Only cells actually drawn in subsequent passes survive. A different
        // horizontal projection starts with no stale physical-column mappings.
        viewport.begin(fields.clone());
        viewport.lookup(85);
        viewport.begin(fields);
        assert_eq!(viewport.rows.len(), 1);
        assert!(viewport.begin(vec![3, 7]));
        assert!(viewport.rows.is_empty());
        Ok(())
    }

    #[test]
    fn cache_evicts_by_bytes_and_count_and_keeps_recently_viewed_pages() -> anyhow::Result<()> {
        let strings = StringViewArray::from(vec!["x".repeat(PAGE_BYTES)]);
        let batch = arrow::record_batch::RecordBatch::try_from_iter([(
            "name",
            Arc::new(strings) as arrow::array::ArrayRef,
        )])?;
        let table = ArrowTable::new(
            crate::arrow_types::columns_from_schema(&batch.schema()),
            vec![batch],
        )?;
        let max_resident = CACHE_BYTES / table.memory_size();
        let mut cache = PageCache::default();
        for start in 0..max_resident {
            cache.insert(Page {
                start,
                data: table.clone(),
            });
        }
        assert!(cache.lookup(0).is_some());
        cache.insert(Page {
            start: max_resident,
            data: table,
        });
        assert!(cache.lookup(1).is_none());
        assert_eq!(cache.lookup(0).unwrap().1, 0);
        assert!(cache.bytes <= CACHE_BYTES);

        for i in 0..CACHE_PAGES * 2 {
            cache.insert(Page {
                start: i,
                data: ArrowTable::default(),
            });
        }
        assert_eq!(cache.pages.len(), CACHE_PAGES);
        assert_eq!(cache.bytes, 0);
        Ok(())
    }

    #[test]
    fn copy_rereads_exact_wide_cells_across_pages_in_visual_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("browse.csv");
        let long = "multi\nline\tfield \"a\" ".repeat(2200);
        let mut csv = String::from("id,description\n");
        for row in 0..600 {
            let text = if row == 500 { &long } else { "short" };
            csv.push_str(&format!("{row},\"{}\"\n", text.replace('"', "\"\"")));
        }
        std::fs::write(&path, csv)?;
        let cancel = AtomicBool::new(false);
        let mut engine = Engine::open_csv(&path, None, &cancel)?;
        let (reader, _, result) = engine.open_csv_view(&cancel)?;
        let columns = result
            .columns
            .into_iter()
            .enumerate()
            .map(|(i, c)| (i, c, FloatDisplay::Standard))
            .collect::<Vec<_>>();
        let order = crate::table_state::RowOrder::new(600, vec![500]);
        let output = copy_rows(
            &reader,
            &engine.resource_client(),
            &cancel,
            &order,
            0..=511,
            &columns,
            true,
        )?;
        let mut expected = format!("id\tdescription\n500\t{}", tsv(&long));
        for row in (0..=511).filter(|row| *row != 500) {
            expected.push_str(&format!("\n{row}\tshort"));
        }
        assert_eq!(output, expected);
        let reversed: Vec<_> = columns.iter().rev().cloned().collect();
        assert_eq!(
            copy_rows(
                &reader,
                &engine.resource_client(),
                &cancel,
                &order,
                1..=1,
                &reversed,
                true
            )?,
            "description\tid\nshort\t0"
        );
        let enormous = crate::table_state::RowOrder::new(300_000_000, vec![]);
        let error = copy_rows(
            &reader,
            &engine.resource_client(),
            &cancel,
            &enormous,
            0..=299_999_999,
            &columns,
            false,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), CLIPBOARD_LIMIT);
        Ok(())
    }
}
