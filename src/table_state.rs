//! View-only freezing and marking for one result snapshot.
//!
//! Indices always refer to the unchanged result data. Frozen rows/columns are
//! separate visual panes, and each pane preserves the supplied source order.
//! Call `clear_rows` when a query produces a new snapshot, `clear_all` when the
//! source changes, and `reconcile` after the available rows/schema change.

use std::collections::{BTreeSet, HashMap, HashSet};

/// Prepare widths for a committed result without changing the displayed one.
/// Field identity, rather than column position or count, determines reuse.
pub(crate) fn reconcile_column_widths(
    previous_names: &[&str],
    previous_widths: &[f32],
    next_names: &[&str],
) -> Vec<f32> {
    let previous = previous_names
        .iter()
        .copied()
        .zip(previous_widths.iter().copied())
        .filter(|(_, width)| width.is_finite() && (70. ..=1500.).contains(width))
        .collect::<HashMap<_, _>>();
    next_names
        .iter()
        .map(|name| {
            previous.get(name).copied().unwrap_or_else(|| {
                ((name.chars().count().clamp(11, 35) as f32) * 7.4 + 28.).clamp(110., 280.)
            })
        })
        .collect()
}

/// Frozen rows first, then all remaining rows in source order. Storage depends
/// only on the number of frozen rows, even for billion-row result snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowOrder {
    row_count: usize,
    frozen: Vec<usize>,
}

impl RowOrder {
    pub fn new(row_count: usize, mut frozen: Vec<usize>) -> Self {
        frozen.retain(|row| *row < row_count);
        frozen.sort_unstable();
        frozen.dedup();
        Self { row_count, frozen }
    }

    pub fn len(&self) -> usize {
        self.row_count
    }

    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    pub fn clear(&mut self) {
        self.row_count = 0;
        self.frozen.clear();
    }

    pub fn frozen(&self) -> &[usize] {
        &self.frozen
    }

    pub fn get(&self, view: usize) -> Option<usize> {
        self.source(view)
    }

    /// Translate a display position without constructing the unpinned body.
    pub fn source(&self, view: usize) -> Option<usize> {
        if view >= self.row_count {
            return None;
        }
        let count = self.frozen.len();
        if view < count {
            return Some(self.frozen[view]);
        }
        let rank = view - count;
        // For frozen source f[i], f[i] - i is its insertion position among
        // the body rows. Every insertion at or before rank shifts it by one.
        let mut low = 0;
        let mut high = count;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.frozen[mid] - mid <= rank {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Some(rank + low)
    }

    /// Inverse mapping: membership and the number of preceding frozen rows
    /// follow from one binary search.
    pub fn view(&self, source: usize) -> Option<usize> {
        if source >= self.row_count {
            return None;
        }
        Some(match self.frozen.binary_search(&source) {
            Ok(index) => index,
            Err(before) => source - before + self.frozen.len(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkColor {
    Blue,
    Teal,
    Amber,
}

impl MarkColor {
    pub fn all() -> &'static [Self] {
        &[Self::Blue, Self::Teal, Self::Amber]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Blue => "Blue",
            Self::Teal => "Teal",
            Self::Amber => "Amber",
        }
    }

    /// A saturated cell fill that retains the theme's text, muted and link
    /// foregrounds. Combine it with `accent` for a clearly visible color marker.
    pub fn background(self, dark: bool) -> u32 {
        match (self, dark) {
            (Self::Blue, false) => 0xe0e5ff,
            (Self::Teal, false) => 0xc4f1e5,
            (Self::Amber, false) => 0xffe3a5,
            (Self::Blue, true) => 0x363f82,
            (Self::Teal, true) => 0x004e47,
            (Self::Amber, true) => 0x603e00,
        }
    }

    /// The row-number edge / column-header underline or menu swatch color.
    /// This is a graphic marker, not a replacement text foreground.
    pub fn accent(self, dark: bool) -> u32 {
        match (self, dark) {
            (Self::Blue, false) => 0x5b5bd6,
            (Self::Teal, false) => 0x007a62,
            (Self::Amber, false) => 0x986300,
            (Self::Blue, true) => 0xa899ff,
            (Self::Teal, true) => 0x3ce6bd,
            (Self::Amber, true) => 0xffca55,
        }
    }

    /// Preserve the mark's hue in selected cells. The renderer supplies a fine
    /// theme-accent border so selection remains distinct from marking alone.
    pub fn selection_background(self, dark: bool) -> u32 {
        match (self, dark) {
            (Self::Blue, false) => 0xd7e2ff,
            (Self::Teal, false) => 0xaff3df,
            (Self::Amber, false) => 0xffe1a0,
            (Self::Blue, true) => 0x333f88,
            (Self::Teal, true) => 0x004f45,
            (Self::Amber, true) => 0x623e00,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableState {
    pinned_rows: BTreeSet<usize>,
    pinned_columns: BTreeSet<String>,
    row_marks: HashMap<usize, MarkColor>,
    column_marks: HashMap<String, MarkColor>,
}

impl TableState {
    pub fn pin_rows(&mut self, rows: impl IntoIterator<Item = usize>) {
        self.pinned_rows.extend(rows);
    }

    pub fn unpin_rows(&mut self, rows: impl IntoIterator<Item = usize>) {
        for row in rows {
            self.pinned_rows.remove(&row);
        }
    }

    pub fn pin_columns<I, S>(&mut self, columns: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.pinned_columns
            .extend(columns.into_iter().map(|column| column.as_ref().to_owned()));
    }

    pub fn unpin_columns<I, S>(&mut self, columns: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for column in columns {
            self.pinned_columns.remove(column.as_ref());
        }
    }

    pub fn is_row_pinned(&self, row: usize) -> bool {
        self.pinned_rows.contains(&row)
    }

    pub fn is_column_pinned(&self, column: &str) -> bool {
        self.pinned_columns.contains(column)
    }

    pub fn clear_row_pins(&mut self) {
        self.pinned_rows.clear();
    }

    pub fn clear_column_pins(&mut self) {
        self.pinned_columns.clear();
    }

    /// Clear freezing while keeping the independent row/column color marks.
    pub fn clear_pins(&mut self) {
        self.clear_row_pins();
        self.clear_column_pins();
    }

    /// Set a row's mark, or remove it with `None`. Freezing is independent.
    pub fn mark_rows(&mut self, rows: impl IntoIterator<Item = usize>, color: Option<MarkColor>) {
        for row in rows {
            if let Some(color) = color {
                self.row_marks.insert(row, color);
            } else {
                self.row_marks.remove(&row);
            }
        }
    }

    /// Set named column marks, or remove them with `None`.
    pub fn mark_columns<I, S>(&mut self, columns: I, color: Option<MarkColor>)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for column in columns {
            if let Some(color) = color {
                self.column_marks.insert(column.as_ref().to_owned(), color);
            } else {
                self.column_marks.remove(column.as_ref());
            }
        }
    }

    pub fn row_mark(&self, row: usize) -> Option<MarkColor> {
        self.row_marks.get(&row).copied()
    }

    pub fn column_mark(&self, column: &str) -> Option<MarkColor> {
        self.column_marks.get(column).copied()
    }

    /// Frozen row indices in their original result order. Ignore stale indices
    /// even if the caller has not reconciled yet.
    pub fn frozen_rows(&self, row_count: usize) -> Vec<usize> {
        self.pinned_rows.range(..row_count).copied().collect()
    }

    /// Complete visual order with storage only for the frozen rows.
    pub fn row_order(&self, row_count: usize) -> RowOrder {
        RowOrder::new(row_count, self.frozen_rows(row_count))
    }

    /// Return `(frozen, body)` column indices. Names select membership, never
    /// order: both panes follow `columns`, including after user reordering.
    pub fn column_order(&self, columns: &[String]) -> (Vec<usize>, Vec<usize>) {
        (0..columns.len()).partition(|index| self.pinned_columns.contains(&columns[*index]))
    }

    /// Drop stale result indices and names absent from the supplied schema.
    /// Pass the full available schema to retain marks on temporarily hidden
    /// columns, or only the visible schema to remove those marks as well.
    pub fn reconcile(&mut self, row_count: usize, columns: &[String]) {
        self.pinned_rows.retain(|row| *row < row_count);
        self.row_marks.retain(|row, _| *row < row_count);
        let available = columns.iter().map(String::as_str).collect::<HashSet<_>>();
        self.pinned_columns
            .retain(|column| available.contains(column.as_str()));
        self.column_marks
            .retain(|column, _| available.contains(column.as_str()));
    }

    /// New result snapshot: row indices have changed meaning. Named column
    /// state remains valid for the same source and is preserved.
    pub fn clear_rows(&mut self) {
        self.pinned_rows.clear();
        self.row_marks.clear();
    }

    /// Exchange only row-local decorations when switching between detail views.
    pub fn swap_rows(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.pinned_rows, &mut other.pinned_rows);
        std::mem::swap(&mut self.row_marks, &mut other.row_marks);
    }

    /// New source: no positional or named state carries over.
    pub fn clear_all(&mut self) {
        self.clear_pins();
        self.row_marks.clear();
        self.column_marks.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Colors;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn group_detail_row_swap_preserves_base_rows_and_live_column_state() {
        let mut table = TableState::default();
        let mut original_rows = TableState::default();
        table.pin_rows([4]);
        table.mark_rows([7], Some(MarkColor::Teal));
        table.pin_columns(["id"]);

        table.swap_rows(&mut original_rows);
        assert!(!table.is_row_pinned(4));
        assert_eq!(table.row_mark(7), None);
        table.pin_rows([0]);
        table.mark_columns(["id"], Some(MarkColor::Amber));

        table.swap_rows(&mut original_rows);
        assert!(!table.is_row_pinned(0));
        assert!(table.is_row_pinned(4));
        assert_eq!(table.row_mark(7), Some(MarkColor::Teal));
        assert!(table.is_column_pinned("id"));
        assert_eq!(table.column_mark("id"), Some(MarkColor::Amber));
    }

    #[test]
    fn result_widths_follow_field_identity_for_hide_add_reorder_and_replacement() {
        let previous_names = ["id", "details", "photo"];
        let previous_widths = vec![88., 320., 600.];
        for (next, expected) in [
            (vec!["id", "details", "photo"], vec![88., 320., 600.]),
            (vec!["id"], vec![88.]),
            (vec!["photo", "id"], vec![600., 88.]),
            (vec!["id", "new", "photo"], vec![88., 110., 600.]),
            (
                vec!["id", "details", "photo", "new"],
                vec![88., 320., 600., 110.],
            ),
            (Vec::new(), Vec::new()),
        ] {
            assert_eq!(
                reconcile_column_widths(&previous_names, &previous_widths, &next),
                expected,
            );
            // Pending, failed and cancelled work keeps using this geometry;
            // callers publish the returned vector only alongside a new result.
            assert_eq!(previous_widths, [88., 320., 600.]);
        }
    }

    #[test]
    fn result_widths_use_existing_defaults_for_new_missing_or_invalid_widths() {
        let names = ["nan", "infinite", "small", "large", "minimum", "maximum"];
        let widths = [f32::NAN, f32::INFINITY, 69., 1501., 70., 1500.];
        assert_eq!(
            reconcile_column_widths(&names, &widths, &names),
            [110., 110., 110., 110., 70., 1500.],
        );
        assert_eq!(
            reconcile_column_widths(&["id", "details"], &[240.], &["details", "id"]),
            [110., 240.],
        );
        let long_unicode = "字段".repeat(20);
        assert_eq!(
            reconcile_column_widths(&[], &[], &["id", &long_unicode]),
            [110., 280.],
        );
    }

    fn assert_row_order(row_count: usize, pinned: Vec<usize>) {
        let pins = pinned
            .iter()
            .copied()
            .filter(|row| *row < row_count)
            .collect::<BTreeSet<_>>();
        let expected = pins
            .iter()
            .copied()
            .chain((0..row_count).filter(|row| !pins.contains(row)))
            .collect::<Vec<_>>();
        let order = RowOrder::new(row_count, pinned);
        assert_eq!(order.len(), row_count);
        assert_eq!(order.is_empty(), row_count == 0);
        assert_eq!(order.frozen(), pins.into_iter().collect::<Vec<_>>());
        for (view, source) in expected.into_iter().enumerate() {
            assert_eq!(order.get(view), Some(source));
            assert_eq!(order.source(view), Some(source));
            assert_eq!(order.view(source), Some(view));
        }
        assert_eq!(order.source(row_count), None);
        assert_eq!(order.view(row_count), None);
        assert_eq!(order.source(usize::MAX), None);
        assert_eq!(order.view(usize::MAX), None);
    }

    #[test]
    fn compact_row_order_handles_empty_all_frozen_adjacent_and_stale_rows() {
        assert_row_order(0, vec![0, 1, usize::MAX]);
        assert_row_order(1, vec![]);
        assert_row_order(1, vec![0, 0]);
        assert_row_order(5, vec![4, 2, 1, 0, 3]);
        assert_row_order(8, vec![0, 1, 4, 5, 7, 4, 8, usize::MAX]);
        assert_row_order(8, vec![7]);
        assert_row_order(8, vec![0]);
        let mut order = RowOrder::new(8, vec![2, 3]);
        order.clear();
        assert_eq!(order.len(), 0);
        assert!(order.is_empty());
        assert!(order.frozen().is_empty());
        assert_eq!(order.source(0), None);
        assert_eq!(order.view(0), None);
    }

    #[test]
    fn compact_row_order_matches_materialized_order_for_random_freezing_patterns() {
        // Deterministic differential testing also exercises long consecutive
        // pin runs, for which several body insertion ranks are identical.
        let mut seed = 0x8f3d_2a61_94bc_07e5_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for row_count in 0..128 {
            for _ in 0..16 {
                let pinned = (0..row_count + 8)
                    .filter_map(|_| {
                        let value = next();
                        (value & 3 != 0).then_some(value as usize % (row_count + 8))
                    })
                    .collect();
                assert_row_order(row_count, pinned);
            }
        }
    }

    #[test]
    fn billion_row_order_stores_only_frozen_indices_and_maps_distant_rows() {
        let row_count = 1_000_000_000;
        let identity = RowOrder::new(row_count, vec![]);
        assert_eq!(identity.frozen.capacity(), 0);
        assert_eq!(identity.source(row_count - 1), Some(row_count - 1));
        assert_eq!(identity.view(row_count - 1), Some(row_count - 1));
        let order = RowOrder::new(row_count, vec![0, 500_000_000, row_count - 1]);
        assert_eq!(order.frozen.capacity(), 3);
        assert_eq!(
            std::mem::size_of_val(&order),
            std::mem::size_of_val(&identity)
        );
        assert_eq!(order.source(0), Some(0));
        assert_eq!(order.source(1), Some(500_000_000));
        assert_eq!(order.source(2), Some(row_count - 1));
        assert_eq!(order.source(3), Some(1));
        assert_eq!(order.source(500_000_002), Some(500_000_001));
        assert_eq!(order.source(row_count - 1), Some(row_count - 2));
        assert_eq!(order.view(row_count - 2), Some(row_count - 1));
        assert_eq!(order.view(500_000_001), Some(500_000_002));
    }

    #[test]
    fn compact_row_order_avoids_overflow_at_usize_limits() {
        let order = RowOrder::new(usize::MAX, vec![0, 1, usize::MAX - 2]);
        assert_eq!(order.source(2), Some(usize::MAX - 2));
        assert_eq!(order.source(3), Some(2));
        assert_eq!(order.source(usize::MAX - 1), Some(usize::MAX - 1));
        assert_eq!(order.view(usize::MAX - 1), Some(usize::MAX - 1));
        assert_eq!(order.source(usize::MAX), None);
        assert_eq!(order.view(usize::MAX), None);
    }

    #[test]
    fn arbitrary_frozen_rows_are_exact_and_each_pane_preserves_source_order() {
        let source = ["zero", "one", "two", "three", "four", "five"];
        let original = source;
        let mut state = TableState::default();
        state.pin_rows([4, 1, 4, 99]);
        assert_eq!(state.frozen_rows(source.len()), [1, 4]);
        let order = state.row_order(source.len());
        assert_eq!(
            (order.frozen().len()..order.len())
                .map(|view| order.source(view).unwrap())
                .collect::<Vec<_>>(),
            [0, 2, 3, 5]
        );
        assert_eq!(
            state
                .frozen_rows(source.len())
                .into_iter()
                .map(|row| source[row])
                .collect::<Vec<_>>(),
            ["one", "four"]
        );
        assert_eq!(source, original);
        state.unpin_rows([1, 1, 300]);
        assert_eq!(state.frozen_rows(source.len()), [4]);
        assert!(state.is_row_pinned(4));
        assert!(!state.is_row_pinned(1));
        assert!(state.frozen_rows(0).is_empty());
        assert!(state.row_order(0).is_empty());
    }

    #[test]
    fn named_frozen_columns_follow_schema_order_and_match_names_exactly() {
        let columns = names(&["z", "中文", "a", "A", "quoted \" name"]);
        let original = columns.clone();
        let mut state = TableState::default();
        state.pin_columns(["quoted \" name", "中文", "中文", "A", "missing"]);
        assert_eq!(state.column_order(&columns), (vec![1, 3, 4], vec![0, 2]));
        assert_eq!(columns, original);
        assert!(state.is_column_pinned("A"));
        assert!(!state.is_column_pinned("a"));
        let reordered = names(&["quoted \" name", "a", "中文", "z", "A"]);
        assert_eq!(state.column_order(&reordered), (vec![0, 2, 4], vec![1, 3]));
        state.unpin_columns(["中文".to_owned(), "missing".to_owned()]);
        assert_eq!(state.column_order(&columns), (vec![3, 4], vec![0, 1, 2]));
        assert_eq!(state.column_order(&[]), (vec![], vec![]));
    }

    #[test]
    fn pinning_and_marking_are_independent_and_duplicate_actions_are_idempotent() {
        let mut state = TableState::default();
        state.pin_rows([2, 2]);
        state.pin_columns(["name", "name"]);
        state.mark_rows([2, 2, 5], Some(MarkColor::Blue));
        state.mark_columns(["name", "name"], Some(MarkColor::Teal));
        let once = state.clone();
        state.pin_rows([2]);
        state.pin_columns(["name"]);
        state.mark_rows([2, 5], Some(MarkColor::Blue));
        state.mark_columns(["name"], Some(MarkColor::Teal));
        assert_eq!(state, once);
        state.unpin_rows([2]);
        state.unpin_columns(["name"]);
        assert_eq!(state.row_mark(2), Some(MarkColor::Blue));
        assert_eq!(state.column_mark("name"), Some(MarkColor::Teal));
        state.mark_rows([2], Some(MarkColor::Amber));
        state.mark_columns(["name"], Some(MarkColor::Blue));
        assert_eq!(state.row_mark(2), Some(MarkColor::Amber));
        assert_eq!(state.column_mark("name"), Some(MarkColor::Blue));
        state.pin_rows([2]);
        state.pin_columns(["name"]);
        state.mark_rows([2, 100], None);
        state.mark_columns(["name", "absent"], None);
        assert!(state.is_row_pinned(2));
        assert!(state.is_column_pinned("name"));
        assert_eq!(state.row_mark(2), None);
        assert_eq!(state.row_mark(5), Some(MarkColor::Blue));
        assert_eq!(state.column_mark("name"), None);
    }

    #[test]
    fn clearing_one_pin_dimension_or_all_pins_preserves_marks() {
        let mut state = TableState::default();
        state.pin_rows([1, 3]);
        state.pin_columns(["first", "second"]);
        state.mark_rows([1], Some(MarkColor::Amber));
        state.mark_columns(["second"], Some(MarkColor::Teal));
        state.clear_row_pins();
        assert!(state.frozen_rows(4).is_empty());
        assert!(state.is_column_pinned("first"));
        assert_eq!(state.row_mark(1), Some(MarkColor::Amber));
        state.pin_rows([3]);
        state.clear_column_pins();
        assert!(state.is_row_pinned(3));
        assert!(!state.is_column_pinned("second"));
        assert_eq!(state.column_mark("second"), Some(MarkColor::Teal));
        state.pin_columns(["first"]);
        state.clear_pins();
        assert!(state.frozen_rows(4).is_empty());
        assert!(!state.is_column_pinned("first"));
        assert_eq!(state.row_mark(1), Some(MarkColor::Amber));
        assert_eq!(state.column_mark("second"), Some(MarkColor::Teal));
    }

    #[test]
    fn reconcile_drops_stale_positions_and_names_without_reassigning_state() {
        let mut state = TableState::default();
        state.pin_rows([0, 2, 5, usize::MAX]);
        state.pin_columns(["keep", "gone", "Keep"]);
        state.mark_rows([1, 2, 5], Some(MarkColor::Blue));
        state.mark_columns(["keep", "gone", "Keep"], Some(MarkColor::Amber));
        let columns = names(&["new", "keep"]);
        state.reconcile(3, &columns);
        assert_eq!(state.frozen_rows(100), [0, 2]);
        assert!(!state.is_row_pinned(usize::MAX));
        assert_eq!(state.row_mark(5), None);
        assert_eq!(state.row_mark(2), Some(MarkColor::Blue));
        assert_eq!(state.column_order(&columns), (vec![1], vec![0]));
        assert_eq!(state.column_mark("keep"), Some(MarkColor::Amber));
        assert_eq!(state.column_mark("gone"), None);
        assert_eq!(state.column_mark("Keep"), None);
        assert_eq!(state.column_mark("new"), None);
        state.reconcile(0, &[]);
        assert_eq!(state, TableState::default());
    }

    #[test]
    fn new_snapshot_clears_row_identity_but_new_source_clears_everything() {
        let mut state = TableState::default();
        state.pin_rows([0, 8]);
        state.pin_columns(["stable"]);
        state.mark_rows([0], Some(MarkColor::Teal));
        state.mark_columns(["stable"], Some(MarkColor::Amber));
        state.clear_rows();
        let order = state.row_order(3);
        assert_eq!(
            (0..order.len())
                .map(|view| order.source(view).unwrap())
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(!state.is_row_pinned(8));
        assert_eq!(state.row_mark(0), None);
        assert!(state.is_column_pinned("stable"));
        assert_eq!(state.column_mark("stable"), Some(MarkColor::Amber));
        state.clear_all();
        assert_eq!(state, TableState::default());
    }

    fn luminance(color: u32) -> f64 {
        let linear = |byte: u32| {
            let value = byte as f64 / 255.;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear((color >> 16) & 0xff)
            + 0.7152 * linear((color >> 8) & 0xff)
            + 0.0722 * linear(color & 0xff)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let a = luminance(a);
        let b = luminance(b);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn marked_cells_keep_real_theme_text_nulls_and_links_readable_in_both_modes() {
        for dark in [false, true] {
            let colors = Colors::new(dark);
            for &mark in MarkColor::all() {
                for (state, background) in [
                    ("normal", mark.background(dark)),
                    ("selected", mark.selection_background(dark)),
                ] {
                    for (name, foreground) in [
                        ("normal text", colors.text),
                        ("NULL and secondary text", colors.muted),
                        ("nested table links", colors.accent),
                    ] {
                        let ratio = contrast(foreground, background);
                        assert!(
                            ratio >= 4.5,
                            "{mark:?}, {state}, dark={dark}, {name}: {ratio:.3}:1 is below WCAG AA normal text"
                        );
                    }
                    for (name, canvas, minimum) in [
                        ("surface", colors.surface, if dark { 1.7 } else { 1.2 }),
                        ("stripe", colors.stripe, if dark { 1.55 } else { 1.15 }),
                    ] {
                        let ratio = contrast(background, canvas);
                        assert!(
                            ratio >= minimum,
                            "{mark:?}, {state}, dark={dark}: fill separates from {name} by only {ratio:.3}:1 (minimum {minimum})"
                        );
                    }
                    let ratio = contrast(mark.accent(dark), background);
                    assert!(
                        ratio >= 3.,
                        "{mark:?}, {state}, dark={dark}: graphic marker contrast {ratio:.3}:1 is below 3:1"
                    );
                }
            }
        }
    }
}
