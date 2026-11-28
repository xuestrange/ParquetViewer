use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use crate::controls::{self, ButtonStyle};
use gpui::{
    App, Application, Bounds, ClipboardItem, Context, Entity, ExternalPaths, FocusHandle,
    Focusable, Image, ImageFormat, KeyBinding, ListAlignment, ListState, Menu, MenuItem,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels,
    ScrollHandle, SharedString, Subscription, TextRun, Window, WindowBounds, WindowOptions,
    actions, canvas, div, img, list, point, prelude::*, px, rgb, rgba, size,
};
use serde::{Deserialize, Serialize};

use crate::{
    aggregation_editor::{AggregationEditor, AggregationEvent},
    audio::AudioPlayer,
    engine::{
        ColumnInfo, DatasetInfo, Engine, ExportFormat, QueryPhase, QueryProgress, QueryRequest,
        QueryResult,
        csv_source::{CsvOptions, CsvPrepared, CsvProbe},
        has_delimited_text_extension, is_parquet_file,
    },
    filter_editor::{FilterEditor, FilterEditorEvent},
    filtering::FilterGroup,
    formatting::{BinaryFormat, FloatDisplay, count_label},
    i18n::{self, Language, t},
    icons::{self, Icon},
    peek::{PeekData, PeekEvent, PeekKey, PeekView},
    source_loader::SourceStamps,
    table_state::TableState,
    text_input::{self, InputEvent, TextInput},
    theme::{self, Colors, ThemeMode},
    window_chrome,
};

#[path = "aggregation_controls.rs"]
mod aggregation_controls;
#[path = "csv_browse.rs"]
mod csv_browse;
#[path = "csv_controls.rs"]
mod csv_controls;
#[path = "csv_jump.rs"]
mod csv_jump;
#[path = "csv_open.rs"]
mod csv_open;
use csv_open::BrowseCommand;
#[path = "export_controls.rs"]
mod export_controls;
#[path = "fields_controls.rs"]
mod fields_controls;
#[path = "filter_controls.rs"]
mod filter_controls;
#[path = "profile_controls.rs"]
mod profile_controls;
#[path = "saved_views.rs"]
mod saved_views;
#[path = "settings_menu.rs"]
mod settings_menu;
#[path = "sort_controls.rs"]
mod sort_controls;
#[path = "status_bar.rs"]
mod status_bar;
#[path = "table_interactions.rs"]
mod table_interactions;
#[path = "table_view.rs"]
mod table_view;
use table_interactions::{Axis, AxisMenu, SelectionKind};
#[path = "statistics_sidebar.rs"]
pub(crate) mod statistics_sidebar;
use statistics_sidebar::{StatisticsTab, sidebar_width};
#[path = "toolbar.rs"]
mod toolbar;
#[path = "welcome.rs"]
mod welcome;

actions!(
    viewer,
    [
        OpenFile,
        ImportTextFile,
        New,
        NewWindow,
        CloseFile,
        Reload,
        Export,
        OpenSavedViews,
        Fields,
        OpenStatistics,
        Execute,
        LoadRange,
        LoadAll,
        BrowseText,
        GoToCsvRow,
        Copy,
        CopyHeaders,
        SelectAll,
        Inspect,
        Help,
        About,
        Dismiss,
        Quit,
        FocusFilter,
        CloseWindow,
        MainWindow,
        NextWindow,
        PreviousWindow,
        English,
        SimplifiedChinese,
    ]
);

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredPreferences")]
struct Preferences {
    theme_mode: ThemeMode,
    // Retain the effective value for existing rendering code and older builds.
    dark: bool,
    recent: Vec<PathBuf>,
    language: Language,
    profile_chart_max_rows: usize,
    profile_chart_category_bars: usize,
}

/// Distinguish a legacy explicit `dark` choice from a fresh configuration.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredPreferences {
    theme_mode: Option<ThemeMode>,
    dark: Option<bool>,
    recent: Vec<PathBuf>,
    language: Language,
    profile_chart_max_rows: Option<usize>,
    profile_chart_category_bars: Option<usize>,
}

impl Default for Preferences {
    fn default() -> Self {
        StoredPreferences::default().into()
    }
}

impl From<StoredPreferences> for Preferences {
    fn from(stored: StoredPreferences) -> Self {
        let theme_mode = stored.theme_mode.unwrap_or(match stored.dark {
            Some(true) => ThemeMode::Dark,
            Some(false) => ThemeMode::Light,
            None => ThemeMode::System,
        });
        let last_appearance = if stored.dark.unwrap_or(false) {
            gpui::WindowAppearance::Dark
        } else {
            gpui::WindowAppearance::Light
        };
        Self {
            theme_mode,
            dark: theme_mode.resolve(last_appearance),
            recent: stored.recent,
            language: stored.language,
            profile_chart_max_rows: stored
                .profile_chart_max_rows
                .unwrap_or(settings_menu::DEFAULT_PROFILE_CHART_MAX_ROWS),
            profile_chart_category_bars: stored
                .profile_chart_category_bars
                .unwrap_or(crate::profile_stats::DEFAULT_CATEGORY_CHART_BARS),
        }
    }
}

/// Only the changed preference is committed, never a window's stale snapshot.
enum PreferenceUpdate {
    Theme(ThemeMode, bool),
    Language(Language),
    ProfileChartLimits {
        rows: Option<usize>,
        category_bars: Option<usize>,
    },
    Recent(Vec<PathBuf>),
}

impl Preferences {
    fn apply_update(&mut self, update: PreferenceUpdate) {
        match update {
            PreferenceUpdate::Theme(mode, dark) => {
                self.theme_mode = mode;
                self.dark = dark;
            }
            PreferenceUpdate::Language(language) => self.language = language,
            PreferenceUpdate::ProfileChartLimits {
                rows,
                category_bars,
            } => {
                if let Some(rows) = rows {
                    self.profile_chart_max_rows = rows;
                }
                if let Some(category_bars) = category_bars {
                    self.profile_chart_category_bars = category_bars;
                }
            }
            PreferenceUpdate::Recent(paths) => {
                for path in paths.into_iter().rev() {
                    self.recent.retain(|item| item != &path);
                    self.recent.insert(0, path);
                }
                self.recent.truncate(8);
            }
        }
    }
}

struct SharedPreferences(Preferences);
impl gpui::Global for SharedPreferences {}

#[derive(Default)]
struct MainWindowRegistry {
    next_color_index: u64,
    last_active: Option<gpui::WindowHandle<Viewer>>,
    process_memory_bytes: Option<u64>,
}
impl gpui::Global for MainWindowRegistry {}

#[derive(Clone, Default, PartialEq)]
struct ColumnStyle {
    binary: BinaryFormat,
    float: FloatDisplay,
    word_wrap: bool,
}

const TABLE_ROW_MIN_HEIGHT: f32 = 28.;
const TABLE_TEXT_LINE_HEIGHT: f32 = 18.;
const TABLE_HEADER_HEIGHT: f32 = 32.;

#[derive(Clone, PartialEq)]
struct RowLayoutKey {
    generation: u64,
    snapshot: String,
    widths: Vec<f32>,
    formats: HashMap<String, ColumnStyle>,
}

impl RowLayoutKey {
    /// Width and formatting changes in a single-line column never alter row
    /// heights. In particular, resizing a million-row table must not rebuild
    /// the list's per-row geometry when all its rows remain the same height.
    fn changes_row_heights(&self, previous: &Self, columns: &[ColumnInfo]) -> bool {
        columns.iter().enumerate().any(|(index, column)| {
            let old = previous.formats.get(&column.name);
            let new = self.formats.get(&column.name);
            let old_wrap = old.is_some_and(|style| style.word_wrap);
            let new_wrap = new.is_some_and(|style| style.word_wrap);
            if old_wrap != new_wrap {
                return true;
            }
            new_wrap
                && (self.snapshot != previous.snapshot
                    || self.widths.get(index) != previous.widths.get(index)
                    || old != new)
        })
    }
}

/// A bounded sample covers both the initial records and the whole loaded
/// result. Auto-sizing must not format every value on the UI thread just
/// because a very large numeric column never reaches the maximum width.
fn column_width_sample_rows(row_count: usize) -> Vec<usize> {
    const LIMIT: usize = 256;
    const HEAD: usize = 64;
    if row_count <= LIMIT {
        return (0..row_count).collect();
    }
    let mut rows: Vec<_> = (0..HEAD).collect();
    let spread = LIMIT - HEAD;
    let span = row_count - 1 - HEAD;
    rows.extend(
        (0..spread)
            .map(|index| HEAD + ((span as u128 * index as u128) / (spread - 1) as u128) as usize),
    );
    rows
}

/// Separate caches are required because GPUI invalidates a list when its width
/// changes. Both panes use identical row heights and synchronize logical offsets
/// only after the source list has finished handling its scroll event.
#[derive(Clone)]
struct RowsScrollState {
    frozen: ListState,
    main: ListState,
}

impl RowsScrollState {
    fn new() -> Self {
        let make = || {
            ListState::new_with_fixed_item_height(
                0,
                ListAlignment::Top,
                px(120.),
                px(TABLE_ROW_MIN_HEIGHT),
            )
        };
        Self {
            frozen: make(),
            main: make(),
        }
    }

    fn reset(&self, count: usize) {
        self.frozen.reset(count);
        self.main.reset(count);
    }

    fn set_fixed_height(&self, fixed: bool) -> bool {
        let height = fixed.then_some(px(TABLE_ROW_MIN_HEIGHT));
        let changed =
            self.main.fixed_item_height() != height || self.frozen.fixed_item_height() != height;
        self.main.set_fixed_item_height(height);
        self.frozen.set_fixed_item_height(height);
        changed
    }

    fn invalidate(&self, count: usize) {
        let mut top = self.main.logical_scroll_top();
        top.offset_in_item = px(0.);
        self.reset(count);
        self.frozen.scroll_to(top);
        self.main.scroll_to(top);
    }

    fn sync_from(&self, frozen: bool) {
        let (source, target) = if frozen {
            (&self.frozen, &self.main)
        } else {
            (&self.main, &self.frozen)
        };
        target.scroll_to(source.logical_scroll_top());
    }

    fn reveal(&self, row: usize) {
        self.main.scroll_to_reveal_item(row);
        self.sync_from(false);
    }

    fn scrollbar_drag_started(&self) {
        self.main.scrollbar_drag_started();
        self.frozen.scrollbar_drag_started();
    }

    fn scrollbar_drag_ended(&self) {
        self.main.scrollbar_drag_ended();
        self.frozen.scrollbar_drag_ended();
    }

    fn set_offset(&self, offset: Pixels) {
        let max = self.main.max_offset_for_scrollbar().height;
        if offset <= px(0.) {
            self.main.scroll_to(gpui::ListOffset::default());
        } else if max > px(0.) && offset >= max - px(1.) {
            // Pixel offsets include estimates for unseen wrapped rows. Anchor
            // after the last item so List::layout_items measures backward from
            // the actual end and fills the viewport, even if its height grows.
            self.main.scroll_to(gpui::ListOffset {
                item_ix: self.main.item_count(),
                offset_in_item: px(0.),
            });
        } else {
            self.main.set_offset_from_scrollbar(point(px(0.), -offset));
        }
        self.sync_from(false);
    }
}

#[derive(Clone)]
enum TableScrollbar {
    Pixels(ScrollHandle),
    Rows(RowsScrollState),
}

impl TableScrollbar {
    fn offset(&self) -> gpui::Point<Pixels> {
        match self {
            Self::Pixels(handle) => handle.offset(),
            Self::Rows(rows) => rows.main.scroll_px_offset_for_scrollbar(),
        }
    }

    fn set_offset(&self, point: gpui::Point<Pixels>) {
        match self {
            Self::Pixels(handle) => handle.set_offset(point),
            Self::Rows(rows) => rows.set_offset(-point.y),
        }
    }

    fn drag_started(&self) {
        if let Self::Rows(rows) = self {
            rows.scrollbar_drag_started();
        }
    }

    fn drag_ended(&self) {
        if let Self::Rows(rows) = self {
            rows.scrollbar_drag_ended();
        }
    }
}

impl Preferences {
    fn path() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("Library/Application Support/ParquetViewer/preferences.json")
    }
    fn load() -> Self {
        let mut preferences: Self = std::fs::read(Self::path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        preferences.recent.retain(|path| !path.is_dir());
        preferences
    }
    fn save(&self) {
        let path = Self::path();
        let result = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap())?;
            crate::source_guard::save_copy(&path, &[], &serde_json::to_vec_pretty(self)?)?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("Could not save preferences: {error}");
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    CsvJump,
    CsvImport,
    Export,
    AggregateExport,
    Sort,
    SavedViews,
    Cell,
    Help,
    About,
    ColumnDisplay { column: usize, binary: bool },
    ProfileChartLimit,
    Range,
}

#[derive(Clone, Copy)]
enum OpenDialog {
    Files,
    SingleFile,
    ParquetFiles,
    TextFile,
}

type Selection = ((usize, usize), (usize, usize));

struct PreparedPeek {
    cell: crate::engine::Cell,
    nested: Option<crate::nested::NestedTable>,
    preview: Option<String>,
    image: Option<Arc<gpui::RenderImage>>,
}

#[derive(Clone, Copy)]
struct SidebarResize {
    start: Pixels,
    width: f32,
    statistics: bool,
    dragged: bool,
}

struct BusyOperation {
    label: &'static str,
    started_at: Instant,
}

impl From<&'static str> for BusyOperation {
    fn from(label: &'static str) -> Self {
        Self {
            label,
            started_at: Instant::now(),
        }
    }
}

pub struct Viewer {
    main_window: Option<gpui::WindowHandle<Viewer>>,
    pub(super) window_color_index: u64,
    focus: FocusHandle,
    visual_filters: FilterGroup,
    displayed_filters: FilterGroup,
    filter_sidebar_open: bool,
    settings_menu: Option<gpui::Point<Pixels>>,
    file_menu_open: bool,
    filter_editor: Option<Entity<FilterEditor>>,
    filter_editor_subscription: Option<Subscription>,
    filter_editor_observer: Option<Subscription>,
    aggregation_editor: Option<Entity<AggregationEditor>>,
    aggregation_subscription: Option<Subscription>,
    aggregation_observer: Option<Subscription>,
    aggregation_inflight: bool,
    aggregation_chart_cancel: Option<Arc<AtomicBool>>,
    aggregation_base: Option<aggregation_controls::AggregationDetail>,
    column_profile: Option<profile_controls::ColumnProfile>,
    profile_field_scroll: gpui::UniformListScrollHandle,
    statistics_tab: Option<StatisticsTab>,
    statistics_scroll: ScrollHandle,
    aggregation_scroll: ScrollHandle,
    statistics_width: Option<f32>,
    sidebar_width: Option<f32>,
    sidebar_resize: Option<SidebarResize>,
    sort_controls: Option<sort_controls::SortControls>,
    saved_views: Option<saved_views::SavedViewsControls>,
    csv_import: Option<csv_controls::CsvImport>,
    csv_options: Option<CsvOptions>,
    csv_index: Option<crate::engine::csv_source::CsvBrowse>,
    csv_browse: Option<csv_browse::CsvBrowse>,
    csv_jump: Entity<TextInput>,
    csv_first_page_rows: usize,
    csv_default_view: bool,
    browse_command: Option<BrowseCommand>,
    csv_index_tick: Option<(u64, bool, bool)>,
    offset: Entity<TextInput>,
    count: Entity<TextInput>,
    field_search: Entity<TextInput>,
    field_scroll: gpui::UniformListScrollHandle,
    fields_sidebar_open: bool,
    detail: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
    engine: Option<Arc<Mutex<Engine>>>,
    interrupt: Option<Arc<crate::engine::InterruptHandle>>,
    info: Option<DatasetInfo>,
    result: Option<Arc<QueryResult>>,
    request: QueryRequest,
    displayed_request: QueryRequest,
    paths: Vec<PathBuf>,
    chosen_fields: Vec<String>,
    fields_explicitly_chosen: bool,
    pending_fields: HashSet<String>,
    widths: Vec<f32>,
    resize: Option<(usize, Pixels, f32)>,
    selection: Option<Selection>,
    selecting: bool,
    selection_kind: SelectionKind,
    axis_menu: Option<AxisMenu>,
    table_state: TableState,
    view_rows: crate::table_state::RowOrder,
    view_columns: Vec<usize>,
    frozen_row_count: usize,
    pinned_scroll: RowsScrollState,
    frozen_horizontal: ScrollHandle,
    panel: Option<Panel>,
    busy: Option<BusyOperation>,
    query_progress: Option<QueryProgress>,
    status_progress_tick: u8,
    error: Option<String>,
    notice: String,
    generation: u64,
    preferences: Preferences,
    scroll: RowsScrollState,
    row_layout_key: Option<RowLayoutKey>,
    row_heights: HashMap<usize, Pixels>,
    wrapped_columns: Vec<usize>,
    display_cache: RefCell<crate::display_cache::DisplayCache>,
    resources: crate::resource_budget::Client,
    window_active: bool,
    preview_request: u64,
    preview_pending: Option<PeekKey>,
    preview_cancel: Option<Arc<AtomicBool>>,
    binary_capabilities: HashMap<usize, Vec<BinaryFormat>>,
    binary_capabilities_pending: std::collections::HashSet<usize>,
    binary_scan_cancel: Arc<AtomicBool>,
    horizontal: ScrollHandle,
    preview_image: Option<Arc<Image>>,
    preview: Option<Entity<PeekView>>,
    preview_key: Option<PeekKey>,
    preview_subscription: Option<Subscription>,
    modified: bool,
    file_stamps: Vec<(PathBuf, Option<(SystemTime, u64)>)>,
    formats: HashMap<String, ColumnStyle>,
    profile_chart_limit: Entity<TextInput>,
    profile_chart_category_limit: Entity<TextInput>,
    nested: Vec<(String, serde_json::Value)>,
    audio: Option<AudioPlayer>,
    audio_cell: Option<(usize, usize)>,
    operation_cancel: Option<Arc<AtomicBool>>,
    frozen_count: usize,
    scroll_drag: Option<usize>,
    selecting_initial_fields: bool,
    audio_seek_drag: bool,
    focused_panel: Option<Panel>,
    selection_menu: Option<gpui::Point<Pixels>>,
}

impl Viewer {
    fn status_activity(&self) -> Option<(&'static str, Instant)> {
        self.busy
            .as_ref()
            .map(|job| (job.label, job.started_at))
            .or_else(|| {
                self.column_profile_started_at()
                    .map(|started| ("Calculating descriptive statistics…", started))
            })
    }

    fn new(window_color_index: u64, cx: &mut Context<Self>) -> Self {
        let preferences = cx.global::<SharedPreferences>().0.clone();
        i18n::set_language(preferences.language);
        let dark = preferences.dark;
        let offset = cx.new(|cx| TextInput::new(cx).with_dark(dark));
        let csv_jump = cx.new(|cx| TextInput::new(cx).with_dark(dark));
        let count = cx.new(|cx| TextInput::new(cx).with_dark(dark));
        let field_search = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(dark)
                .with_placeholder(&t("Search fields, or name1, name2"))
        });
        let detail = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(dark)
                .with_multiline(true)
                .with_fill_height(true)
                .read_only(true)
        });
        let profile_chart_limit = cx.new(|cx| TextInput::new(cx).with_dark(dark));
        let profile_chart_category_limit = cx.new(|cx| TextInput::new(cx).with_dark(dark));
        offset.update(cx, |input, cx| input.set_text("0", cx));
        count.update(cx, |input, cx| input.set_text("1000", cx));
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&csv_jump, |this: &mut Self, _, event, cx| {
            if this.panel == Some(Panel::CsvJump) {
                match event {
                    InputEvent::Submit => this.submit_csv_jump(cx),
                    InputEvent::Changed => cx.notify(),
                }
            }
        }));
        for input in [&offset, &count] {
            subscriptions.push(
                cx.subscribe(input, |this: &mut Self, _, event, cx| match event {
                    InputEvent::Submit => {
                        if this.busy.is_none() {
                            if this.panel == Some(Panel::Range) {
                                this.apply_load_range(cx);
                            } else if this.panel.is_none() {
                                this.execute(cx);
                            }
                        }
                    }
                    InputEvent::Changed => cx.notify(),
                }),
            );
        }
        subscriptions.push(
            cx.subscribe(&field_search, |this: &mut Self, _, event, cx| {
                if matches!(event, InputEvent::Changed) {
                    this.field_scroll = gpui::UniformListScrollHandle::new();
                    cx.notify();
                }
            }),
        );
        for input in [&profile_chart_limit, &profile_chart_category_limit] {
            subscriptions.push(cx.subscribe(input, |this: &mut Self, _, event, cx| {
                if this.panel == Some(Panel::ProfileChartLimit) {
                    match event {
                        InputEvent::Submit => this.apply_profile_chart_limit(cx),
                        InputEvent::Changed => cx.notify(),
                    }
                }
            }));
        }
        subscriptions.push(
            cx.observe_global::<SharedPreferences>(|this: &mut Self, cx| {
                let preferences = cx.global::<SharedPreferences>().0.clone();
                this.sync_preferences(preferences, cx);
            }),
        );
        let this = Self {
            main_window: None,
            window_color_index,
            focus: cx.focus_handle(),
            visual_filters: FilterGroup::default(),
            displayed_filters: FilterGroup::default(),
            filter_sidebar_open: false,
            settings_menu: None,
            file_menu_open: false,
            filter_editor: None,
            filter_editor_subscription: None,
            filter_editor_observer: None,
            aggregation_editor: None,
            aggregation_subscription: None,
            aggregation_observer: None,
            aggregation_inflight: false,
            aggregation_chart_cancel: None,
            aggregation_base: None,
            column_profile: None,
            profile_field_scroll: gpui::UniformListScrollHandle::new(),
            statistics_tab: None,
            statistics_scroll: ScrollHandle::new(),
            aggregation_scroll: ScrollHandle::new(),
            statistics_width: None,
            sidebar_width: None,
            sidebar_resize: None,
            sort_controls: None,
            saved_views: None,
            csv_import: None,
            csv_options: None,
            csv_index: None,
            csv_browse: None,
            csv_jump,
            csv_first_page_rows: 0,
            csv_default_view: false,
            browse_command: None,
            csv_index_tick: None,
            offset,
            count,
            field_search,
            field_scroll: gpui::UniformListScrollHandle::new(),
            fields_sidebar_open: false,
            detail,
            _subscriptions: subscriptions,
            engine: None,
            interrupt: None,
            info: None,
            result: None,
            request: QueryRequest::default(),
            displayed_request: QueryRequest::default(),
            paths: Vec::new(),
            chosen_fields: Vec::new(),
            fields_explicitly_chosen: false,
            pending_fields: HashSet::new(),
            widths: Vec::new(),
            resize: None,
            selection: None,
            selecting: false,
            selection_kind: SelectionKind::Cells,
            axis_menu: None,
            table_state: TableState::default(),
            view_rows: crate::table_state::RowOrder::default(),
            view_columns: Vec::new(),
            frozen_row_count: 0,
            pinned_scroll: RowsScrollState::new(),
            frozen_horizontal: ScrollHandle::new(),
            panel: None,
            busy: None,
            query_progress: None,
            status_progress_tick: 0,
            error: None,
            notice: String::new(),
            generation: 0,
            preferences,
            scroll: RowsScrollState::new(),
            row_layout_key: None,
            row_heights: HashMap::new(),
            wrapped_columns: Vec::new(),
            display_cache: RefCell::new(crate::display_cache::DisplayCache::default()),
            resources: crate::resource_budget::Client::new(),
            window_active: true,
            preview_request: 0,
            preview_pending: None,
            preview_cancel: None,
            binary_capabilities: HashMap::new(),
            binary_capabilities_pending: std::collections::HashSet::new(),
            binary_scan_cancel: Arc::new(AtomicBool::new(false)),
            horizontal: ScrollHandle::new(),
            preview_image: None,
            preview: None,
            preview_key: None,
            preview_subscription: None,
            modified: false,
            file_stamps: Vec::new(),
            formats: HashMap::new(),
            profile_chart_limit,
            profile_chart_category_limit,
            nested: Vec::new(),
            audio: None,
            audio_cell: None,
            operation_cancel: None,
            frozen_count: 0,
            scroll_drag: None,
            selecting_initial_fields: false,
            audio_seek_drag: false,
            focused_panel: None,
            selection_menu: None,
        };
        cx.on_release(|this, cx| {
            this.stop_csv_browse();
            this.close_column_profile();
            this.cancel_aggregation_chart_refresh();
            this.binary_scan_cancel.store(true, Ordering::Relaxed);
            if let Some(cancel) = this.operation_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            if let Some(interrupt) = &this.interrupt {
                interrupt.interrupt();
            }
            this.audio.take();
            if let Some(cancel) = this.preview_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            this.preview.take();
            this.preview_subscription.take();
            if let Some(editor) = this.aggregation_editor.take() {
                editor.update(cx, |editor, cx| editor.clear_result(cx));
            }
            // Arrow buffers and the native connection can own gigabytes.
            // Releasing their final references must not stall another window.
            let result = this.result.take();
            let aggregation_base = this.aggregation_base.take();
            let engine = this.engine.take();
            cx.background_executor()
                .spawn(async move { drop((result, aggregation_base, engine)) })
                .detach();
        })
        .detach();
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if weak
                    .update(cx, |this, cx| {
                        this.poll_csv_index(cx);
                        if this.busy.is_some()
                            || this.column_profile_running()
                            || this.audio.as_ref().is_some_and(|audio| audio.is_playing())
                        {
                            this.status_progress_tick = (this.status_progress_tick + 1) % 10;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(2500))
                    .await;
                let Ok(stamps) = weak.read_with(cx, |this, _| {
                    if this.modified {
                        Vec::new()
                    } else {
                        this.file_stamps.clone()
                    }
                }) else {
                    break;
                };
                if stamps.is_empty() {
                    continue;
                }
                let original = stamps.clone();
                let changed = cx
                    .background_executor()
                    .spawn(async move { stamps.iter().any(|(path, old)| stamp(path) != *old) })
                    .await;
                if weak
                    .update(cx, |this, cx| {
                        if changed && this.file_stamps == original {
                            this.modified = true;
                            this.notice =
                                "Source files changed. Reload to see the latest data (⌘R).".into();
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this
    }

    fn open_dialog(&mut self, kind: OpenDialog, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.file_menu_open) {
            cx.notify();
        }
        let response = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: matches!(kind, OpenDialog::Files | OpenDialog::ParquetFiles),
            prompt: Some(
                t(match kind {
                    OpenDialog::Files | OpenDialog::SingleFile => "Open File…",
                    OpenDialog::ParquetFiles => "Open Parquet File…",
                    OpenDialog::TextFile => "Import Text File…",
                })
                .into(),
            ),
        });
        cx.spawn(async move |weak, cx| match response.await {
            Ok(Ok(Some(paths))) => {
                let _ = weak.update(cx, |this, cx| {
                    if matches!(kind, OpenDialog::SingleFile) {
                        if let [path] = paths.as_slice() {
                            this.open_single_file(path.clone(), cx);
                        }
                    } else if matches!(kind, OpenDialog::TextFile) {
                        if let [path] = paths.as_slice() {
                            this.begin_csv_import(path.clone(), None, cx);
                        } else {
                            this.fail("Choose a single text file.", cx);
                        }
                    } else if matches!(kind, OpenDialog::ParquetFiles)
                        && paths.iter().any(|path| !is_parquet_file(path))
                    {
                        this.fail("Choose a Parquet file.", cx);
                    } else {
                        this.open(paths, cx);
                    }
                });
            }
            Ok(Err(error)) => {
                let _ = weak.update(cx, |this, cx| this.fail(error, cx));
            }
            _ => {}
        })
        .detach();
    }

    fn open(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        if paths.iter().any(|path| path.is_dir()) {
            self.fail("Choose a file instead of a folder.", cx);
            return;
        }
        if let Some(path) = paths.iter().find(|path| has_delimited_text_extension(path)) {
            if paths.len() == 1 {
                self.open_source(vec![path.clone()], None, cx);
            } else {
                self.fail("Text files must be opened individually.", cx);
            }
            return;
        }
        if paths
            .iter()
            .any(|path| path.is_file() && !is_parquet_file(path))
        {
            self.fail("Choose a Parquet or delimited text file.", cx);
            return;
        }
        self.open_source(paths, None, cx);
    }

    fn open_single_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path.is_file() && !has_delimited_text_extension(&path) && !is_parquet_file(&path) {
            self.begin_csv_import(path, None, cx);
        } else {
            self.open(vec![path], cx);
        }
    }

    fn open_source(
        &mut self,
        paths: Vec<PathBuf>,
        text_import: Option<(CsvOptions, CsvPrepared)>,
        cx: &mut Context<Self>,
    ) {
        let files: Vec<_> = paths.iter().filter(|p| p.is_file()).cloned().collect();
        if let Err(error) = crate::source_guard::register_sources(&files) {
            self.fail(error, cx);
            return;
        }
        let csv_import = self.csv_import.take().filter(|_| text_import.is_some());
        self.cancel(cx);
        self.csv_import = csv_import;
        self.busy = Some("Reading source schema…".into());
        self.error = None;
        self.panel = None;
        self.generation += 1;
        let generation = self.generation;
        let requested_paths = paths.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation_cancel = Some(cancel.clone());
        self.query_progress = Some(QueryProgress::default());
        let task = cx.background_executor().spawn(async move {
            let text_import = match (text_import, paths.as_slice()) {
                (None, [path]) if has_delimited_text_extension(path) => {
                    let probe = CsvProbe::read_for_browse(path, &cancel)?;
                    Some((probe.options.clone(), probe.prepared()))
                }
                (configured, _) => configured,
            };
            let (csv_options, prepared) = text_import.unzip();
            let (mut engine, stamps) = read_source(&paths, prepared, &cancel)?;
            let loaded = csv_options
                .as_ref()
                .map(|_| engine.open_csv_view(&cancel))
                .transpose()?;
            Ok::<_, anyhow::Error>((engine, stamps, csv_options, loaded))
        });
        cx.spawn(async move |weak, cx| {
            let mut outcome = Some(task.await);
            let _ = weak.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.busy = None;
                this.query_progress = None;
                this.operation_cancel = None;
                match outcome.take().unwrap() {
                    Ok((engine, stamps, csv_options, loaded)) => {
                        this.stop_csv_browse();
                        this.csv_import = None;
                        this.saved_views = None;
                        this.reset_aggregation_controls(cx);
                        this.close_preview(cx);
                        this.close_fields_sidebar();
                        this.selection_menu = None;
                        this.csv_options = csv_options;
                        this.paths = requested_paths;
                        this.interrupt = Some(engine.interrupt_handle());
                        this.info = Some(engine.info.clone());
                        this.chosen_fields =
                            engine.info.columns.iter().map(|c| c.name.clone()).collect();
                        this.fields_explicitly_chosen = false;
                        this.file_stamps = stamps;
                        this.modified = false;
                        this.audio = None;
                        this.audio_cell = None;
                        this.resources = engine.resource_client();
                        this.resources.set_active(this.window_active);
                        this.display_cache.borrow_mut().clear();
                        this.replace_engine(Some(Arc::new(Mutex::new(engine))), cx);
                        this.request = QueryRequest::default();
                        this.displayed_request = QueryRequest::default();
                        this.replace_loaded_result(None, cx);
                        this.selection = None;
                        this.widths.clear();
                        this.formats.clear();
                        this.table_state.clear_all();
                        this.view_rows.clear();
                        this.view_columns.clear();
                        this.frozen_row_count = 0;
                        this.frozen_count = 0;
                        this.reset_filter_controls(cx);
                        this.field_search
                            .update(cx, |input, cx| input.set_text("", cx));
                        this.offset.update(cx, |input, cx| input.set_text("0", cx));
                        this.update_preferences(PreferenceUpdate::Recent(this.paths.clone()), cx);
                        this.selecting_initial_fields = false;
                        this.notice.clear();
                        if let Some((index, request, result)) = loaded {
                            this.install_csv_first_page(index, &request, &result, cx);
                            this.request = request.clone();
                            this.displayed_request = request;
                            this.reset_table_scroll();
                            this.replace_loaded_result(Some(Arc::new(result)), cx);
                            this.refresh_table_order(cx);
                        } else if this.awaiting_initial_range() {
                            this.count
                                .update(cx, |input, cx| input.set_text("1000", cx));
                            this.panel = Some(Panel::Range);
                        } else {
                            let rows = this.info.as_ref().unwrap().row_count.max(1);
                            this.count
                                .update(cx, |input, cx| input.set_text(&rows.to_string(), cx));
                            this.execute(cx);
                        }
                    }
                    Err(error) => {
                        if this.csv_import.is_some() {
                            this.show_csv_load_error(error, cx);
                        } else if let [path] = requested_paths.as_slice()
                            && has_delimited_text_extension(path)
                        {
                            this.begin_csv_import(path.clone(), None, cx);
                            this.show_csv_load_error(error, cx);
                        } else {
                            this.fail(error, cx);
                        }
                    }
                }
                cx.notify();
            });
            if let Some(stale) = outcome {
                cx.background_executor()
                    .spawn(async move { drop(stale) })
                    .detach();
            }
        })
        .detach();
        cx.notify();
    }

    fn current_request(&mut self, cx: &mut Context<Self>) -> Option<QueryRequest> {
        let offset = self.offset.read(cx).text().trim().parse::<usize>();
        let limit = self.count.read(cx).text().trim().parse::<usize>();
        let (Ok(offset), Ok(limit)) = (offset, limit) else {
            self.error = Some(
                "Offset must be a nonnegative integer; count must be a positive integer.".into(),
            );
            cx.notify();
            return None;
        };
        if limit == 0 {
            self.error = Some("Record count must be greater than zero.".into());
            cx.notify();
            return None;
        }
        let filter = match self.effective_filter() {
            Ok(filter) => filter,
            Err(error) => {
                self.fail(error, cx);
                return None;
            }
        };
        Some(QueryRequest {
            offset,
            limit,
            filter,
            selected_columns: self.chosen_fields.clone(),
            ..self.request.clone()
        })
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.paths.is_empty() {
            return;
        }
        if let (Some(options), [path]) = (&self.csv_options, self.paths.as_slice()) {
            self.begin_csv_import(path.clone(), Some(options.clone()), cx);
            return;
        }
        if self.selecting_initial_fields {
            return;
        }
        let Some(request) = self.current_request(cx) else {
            return;
        };
        self.reload_source(request, self.visual_filters.clone(), None, cx);
    }

    fn reload_source(
        &mut self,
        request: QueryRequest,
        displayed_filters: FilterGroup,
        text_import: Option<(CsvOptions, CsvPrepared)>,
        cx: &mut Context<Self>,
    ) {
        let default_text = self.csv_default_view && text_import.is_some();
        let csv_import = self.csv_import.take().filter(|_| text_import.is_some());
        self.cancel(cx);
        self.csv_import = csv_import;
        self.close_preview(cx);
        self.close_fields_sidebar();
        self.selection_menu = None;
        self.panel = None;
        self.error = None;
        self.busy = Some("Reloading file…".into());
        self.generation += 1;
        let generation = self.generation;
        let paths = self.paths.clone();
        let (csv_options, prepared) = text_import.unzip();
        let reload_filters = displayed_filters.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation_cancel = Some(cancel.clone());
        let progress = QueryProgress::default();
        self.query_progress = Some(progress.clone());
        let task = cx.background_executor().spawn(async move {
            let (mut engine, stamps) = read_source(&paths, prepared, &cancel)?;
            let (index, mut request, result) = if default_text {
                let (index, request, result) = engine.open_csv_view(&cancel)?;
                (Some(index), request, result)
            } else {
                let index = csv_options
                    .as_ref()
                    .map(|_| engine.start_csv_index())
                    .transpose()?;
                let mut request = request;
                request.filter = reload_filters.to_filter(&engine.info.columns)?;
                let result =
                    engine.query_cancellable_with_progress(&request, &cancel, &progress)?;
                (index, request, result)
            };
            if request.load_to_end && result.total_rows_known {
                request.limit = result.total_rows.saturating_sub(request.offset).max(1);
            }
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("Cancelled");
            }
            Ok::<_, anyhow::Error>((engine, stamps, csv_options, request, result, index))
        });
        cx.spawn(async move |weak, cx| {
            let mut outcome = Some(task.await);
            let _ = weak.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.busy = None;
                this.query_progress = None;
                this.operation_cancel = None;
                match outcome.take().unwrap() {
                    Ok((engine, stamps, csv_options, request, result, index)) => {
                        // Publish the replacement engine and its snapshot together.
                        // Invalid filters, changed schema and cancellation keep the old view usable.
                        this.stop_csv_browse();
                        if let Some(index) = index {
                            if default_text {
                                this.install_csv_first_page(index, &request, &result, cx);
                            } else {
                                this.csv_index = Some(index);
                            }
                        }
                        this.csv_import = None;
                        this.csv_options = csv_options;
                        this.interrupt = Some(engine.interrupt_handle());
                        this.info = Some(engine.info.clone());
                        this.resources = engine.resource_client();
                        this.resources.set_active(this.window_active);
                        this.display_cache.borrow_mut().clear();
                        this.replace_engine(Some(Arc::new(Mutex::new(engine))), cx);
                        this.file_stamps = stamps;
                        this.modified = false;
                        this.audio = None;
                        this.audio_cell = None;
                        this.selection = None;
                        this.request = request.clone();
                        if request.load_to_end {
                            this.count.update(cx, |input, cx| {
                                input.set_text(&request.limit.to_string(), cx)
                            });
                        }
                        this.displayed_request = request;
                        this.displayed_filters = displayed_filters;
                        this.notice.clear();
                        this.reset_table_scroll();
                        this.replace_loaded_result(Some(Arc::new(result)), cx);
                        this.table_state.clear_rows();
                        this.refresh_table_order(cx);
                        this.reconcile_filter_editor(cx);
                        this.reconcile_aggregation_result(cx);
                    }
                    Err(error) if this.csv_import.is_some() => this.show_csv_load_error(error, cx),
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
            if let Some(stale) = outcome {
                cx.background_executor()
                    .spawn(async move { drop(stale) })
                    .detach();
            }
        })
        .detach();
        cx.notify();
    }

    fn execute(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.current_request(cx) else {
            return;
        };
        self.execute_request(request, self.visual_filters.clone(), None, cx);
    }

    fn execute_request(
        &mut self,
        request: QueryRequest,
        displayed_filters: FilterGroup,
        restore: Option<saved_views::Restore>,
        cx: &mut Context<Self>,
    ) {
        self.execute_table_request(request, displayed_filters, restore, false, cx);
    }

    fn execute_table_request(
        &mut self,
        request: QueryRequest,
        displayed_filters: FilterGroup,
        mut restore: Option<saved_views::Restore>,
        resume_browse: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        if self.busy.is_some() {
            self.cancel(cx);
        }
        if restore.is_none()
            && self.csv_browse.is_none()
            && !resume_browse
            && !self.modified
            && self.result.is_some()
            && request == self.displayed_request
        {
            // Reapplying an unchanged rule/range must not scan a large dataset
            // again. Explicit Reload has its own path and always reads afresh.
            self.restore_aggregation_details(cx);
            self.request = request;
            self.displayed_filters = displayed_filters;
            self.error = None;
            cx.notify();
            return;
        }
        self.close_preview(cx);
        self.selection_menu = None;
        let restoring = restore.is_some();
        // Leave the current projection and sort available until the new
        // columns can be published together with their Arrow snapshot.
        let choosing_fields = request.selected_columns != self.chosen_fields;
        if !restoring && !choosing_fields {
            self.request = request.clone();
        }
        let mut displayed_request = request.clone();
        self.busy = Some("Loading records…".into());
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        let cleanup_engine = engine.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation_cancel = Some(cancel.clone());
        let progress = QueryProgress::default();
        self.query_progress = Some(progress.clone());
        let displayed_snapshot = self
            .result
            .as_ref()
            .map(|result| result.snapshot_id.clone());
        let task = cx.background_executor().spawn(async move {
            let mut engine = engine
                .lock()
                .map_err(|_| anyhow::anyhow!("Data engine lock was poisoned"))?;
            anyhow::ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
            if let Some(snapshot) = &displayed_snapshot {
                engine.restore_displayed_window(snapshot);
            }
            let result = if resume_browse {
                engine.open_csv_view(&cancel)?.2
            } else {
                engine.query_cancellable_with_progress(&request, &cancel, &progress)?
            };
            if let Some(restore) = &mut restore
                && let Err(error) = restore.prepare_rows(&result, &cancel)
            {
                let _ = engine.discard_snapshot(&result.snapshot_id);
                if let Some(snapshot) = &displayed_snapshot {
                    engine.restore_displayed_window(snapshot);
                }
                return Err(error);
            }
            Ok((result, restore))
        });
        cx.spawn(async move |weak, cx| {
            let mut outcome = Some(task.await);
            let obsolete = weak
                .update(cx, |this, cx| {
                    if this.generation != generation {
                        return None;
                    }
                    this.busy = None;
                    this.query_progress = None;
                    this.operation_cancel = None;
                    let mut obsolete = None;
                    match outcome.take().unwrap() {
                        Ok((result, restore)) => {
                            this.csv_browse = None;
                            this.csv_default_view = resume_browse;
                            if resume_browse && let Some(index) = &this.csv_index {
                                this.csv_browse =
                                    Some(csv_browse::CsvBrowse::new(index.clone(), &result));
                                displayed_request.limit = result.row_count().max(1);
                                this.csv_first_page_rows = displayed_request.limit;
                                this.offset.update(cx, |input, cx| input.set_text("0", cx));
                                this.count.update(cx, |input, cx| {
                                    input.set_text(&displayed_request.limit.to_string(), cx)
                                });
                                this.fields_explicitly_chosen = false;
                                this.reset_filter_controls(cx);
                                this.reset_aggregation_controls(cx);
                                this.close_fields_sidebar();
                                this.close_statistics_sidebar(cx);
                                this.formats.clear();
                                this.table_state.clear_all();
                            }
                            if let Some(info) = &mut this.info {
                                info.row_count = result.total_rows;
                                info.row_count_known = result.total_rows_known;
                            }
                            if displayed_request.load_to_end && result.total_rows_known {
                                displayed_request.limit = result
                                    .total_rows
                                    .saturating_sub(displayed_request.offset)
                                    .max(1);
                                this.count.update(cx, |input, cx| {
                                    input.set_text(&displayed_request.limit.to_string(), cx)
                                });
                            }
                            obsolete = this
                                .result
                                .as_ref()
                                .map(|result| result.snapshot_id.clone());
                            this.request = displayed_request.clone();
                            this.displayed_request = displayed_request;
                            this.displayed_filters = displayed_filters;
                            if choosing_fields && !restoring {
                                this.chosen_fields = result
                                    .columns
                                    .iter()
                                    .map(|column| column.name.clone())
                                    .collect();
                                this.fields_explicitly_chosen = !resume_browse;
                            }
                            this.notice.clear();
                            this.selection = None;
                            this.audio = None;
                            this.audio_cell = None;
                            this.reset_table_scroll();
                            this.replace_loaded_result(Some(Arc::new(result)), cx);
                            this.table_state.clear_rows();
                            if let Some(restore) = restore {
                                this.commit_saved_view(restore, cx);
                            }
                            this.refresh_table_order(cx);
                            if this.csv_index.is_some() {
                                this.csv_index_tick = None;
                                this.poll_csv_index(cx);
                            }
                            this.reconcile_filter_editor(cx);
                            this.reconcile_aggregation_result(cx);
                            this.finish_browse_command(cx);
                        }
                        Err(error) => {
                            if restoring {
                                this.fail(error, cx);
                                return None;
                            }
                            this.restore_displayed_range(cx);
                            if let Some(result) = &this.result {
                                this.chosen_fields = result
                                    .columns
                                    .iter()
                                    .map(|column| column.name.clone())
                                    .collect();
                            }
                            this.reconcile_filter_editor(cx);
                            this.fail(error, cx);
                        }
                    }
                    cx.notify();
                    obsolete
                })
                .ok()
                .flatten();
            // The window may disappear after the worker finishes but before
            // publishing. Release rejected results and the engine off the UI thread.
            cx.background_executor()
                .spawn(async move {
                    let obsolete = obsolete.or_else(|| {
                        outcome
                            .as_ref()
                            .and_then(|result| result.as_ref().ok())
                            .map(|(result, _)| result.snapshot_id.clone())
                    });
                    drop(outcome);
                    if let Some(snapshot) = obsolete {
                        if let Ok(mut engine) = cleanup_engine.lock() {
                            let _ = engine.discard_snapshot(&snapshot);
                        }
                    }
                    drop(cleanup_engine);
                })
                .detach();
        })
        .detach();
        cx.notify();
    }

    fn fail(&mut self, error: impl std::fmt::Display, cx: &mut Context<Self>) {
        self.cancel(cx);
        self.error = Some(format!("{error:#}"));
        self.busy = None;
        self.query_progress = None;
        cx.notify();
    }
    fn observe_system_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = self
            .preferences
            .theme_mode
            .resolve(theme::system_appearance(window.appearance()));
        self.apply_effective_theme(dark, cx);
        let subscription = cx.observe_window_appearance(window, |this, window, cx| {
            if cx.global::<SharedPreferences>().0.theme_mode == ThemeMode::System {
                let dark = ThemeMode::System.resolve(theme::system_appearance(window.appearance()));
                if cx.global::<SharedPreferences>().0.dark != dark {
                    cx.global_mut::<SharedPreferences>().0.dark = dark;
                }
                this.apply_effective_theme(dark, cx);
            }
        });
        self._subscriptions.push(subscription);
    }

    fn set_theme_mode(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        let dark = mode.resolve(theme::system_appearance(cx.window_appearance()));
        self.update_preferences(PreferenceUpdate::Theme(mode, dark), cx);
    }

    fn update_preferences(&mut self, update: PreferenceUpdate, cx: &mut Context<Self>) {
        let preferences = update_shared_preferences(update, cx);
        self.sync_preferences(preferences, cx);
    }

    fn sync_preferences(&mut self, preferences: Preferences, cx: &mut Context<Self>) {
        let dark = preferences.dark;
        let old_dark = self.preferences.dark;
        let language_changed = self.preferences.language != preferences.language;
        let chart_limit_changed = self.preferences.profile_chart_max_rows
            != preferences.profile_chart_max_rows
            || self.preferences.profile_chart_category_bars
                != preferences.profile_chart_category_bars;
        if chart_limit_changed {
            self.sync_chart_limit_inputs(&preferences, cx);
        }
        self.preferences = preferences;
        self.preferences.dark = old_dark;
        self.apply_effective_theme(dark, cx);
        if chart_limit_changed {
            self.refresh_column_profile(cx);
            self.refresh_aggregation_charts(cx);
        }
        if language_changed {
            self.sync_saved_view_input(cx);
            self.sync_sort_inputs(cx);
            self.display_cache.borrow_mut().clear();
            self.field_search.update(cx, |input, cx| {
                input.set_placeholder(&t("Search fields, or name1, name2"), cx)
            });
            if let Some(editor) = &self.filter_editor {
                editor.update(cx, |editor, cx| editor.refresh_language(cx));
            }
            if let Some(editor) = &self.aggregation_editor {
                editor.update(cx, |editor, cx| editor.refresh_language(cx));
            }
            if let Some(preview) = &self.preview {
                preview.update(cx, |peek, cx| peek.refresh_language(cx));
            }
        }
        cx.notify();
    }

    fn apply_effective_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.preferences.dark == dark {
            return;
        }
        self.preferences.dark = dark;
        self.set_csv_import_dark(dark, cx);
        self.sync_saved_view_input(cx);
        self.sync_sort_inputs(cx);
        for input in [
            &self.offset,
            &self.count,
            &self.csv_jump,
            &self.field_search,
            &self.detail,
            &self.profile_chart_limit,
            &self.profile_chart_category_limit,
        ] {
            input.update(cx, |input, cx| input.set_dark(dark, cx));
        }
        if let Some(editor) = &self.filter_editor {
            editor.update(cx, |editor, cx| editor.set_dark(dark, cx));
        }
        if let Some(editor) = &self.aggregation_editor {
            editor.update(cx, |editor, cx| editor.set_dark(dark, cx));
        }
        if let Some(preview) = &self.preview {
            preview.update(cx, |peek, cx| peek.set_dark(dark, cx));
        }
        cx.notify();
    }
    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.browse_command = None;
        self.dismiss_csv_import(cx);
        if self.busy.is_some() {
            if let Some(cancel) = self.operation_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            if let Some(interrupt) = &self.interrupt {
                interrupt.interrupt();
            }
            self.generation += 1;
            self.busy = None;
            self.query_progress = None;
            if self.result.is_some() {
                self.restore_displayed_range(cx);
            }
            self.notice = "Cancelled".into();
            if self.aggregation_inflight {
                self.aggregation_inflight = false;
                if let Some(editor) = &self.aggregation_editor {
                    editor.update(cx, |editor, cx| editor.set_busy(false, cx));
                }
            }
            cx.notify();
        }
    }
    fn close_file(&mut self, cx: &mut Context<Self>) {
        self.stop_csv_browse();
        self.saved_views = None;
        self.file_menu_open = false;
        self.reset_aggregation_controls(cx);
        self.reset_filter_controls(cx);
        self.selection_menu = None;
        self.close_fields_sidebar();
        self.selecting_initial_fields = false;
        self.audio = None;
        self.audio_cell = None;
        self.cancel(cx);
        self.close_preview(cx);
        self.sort_controls = None;
        self.replace_engine(None, cx);
        // Reuse caches while browsing, but release their allocations when
        // the window returns to the welcome screen.
        *self.display_cache.get_mut() = Default::default();
        self.row_layout_key = None;
        self.row_heights = HashMap::new();
        self.binary_capabilities = HashMap::new();
        self.binary_capabilities_pending = HashSet::new();
        self.binary_scan_cancel.store(true, Ordering::Relaxed);
        self.binary_scan_cancel = Arc::new(AtomicBool::new(false));
        self.wrapped_columns = Vec::new();
        self.resources = crate::resource_budget::Client::new();
        self.resources.set_active(self.window_active);
        self.interrupt = None;
        self.info = None;
        self.csv_options = None;
        self.replace_loaded_result(None, cx);
        self.request = QueryRequest::default();
        self.displayed_request = QueryRequest::default();
        self.chosen_fields = Vec::new();
        self.fields_explicitly_chosen = false;
        self.widths = Vec::new();
        self.formats = HashMap::new();
        self.panel = None;
        self.paths.clear();
        self.file_stamps.clear();
        self.selection = None;
        self.notice.clear();
        self.error = None;
        self.modified = false;
        self.table_state = TableState::default();
        self.view_rows = Default::default();
        self.view_columns = Vec::new();
        self.refresh_table_order(cx);
        cx.notify();
    }
    fn load_all(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some()
            || self.selecting_initial_fields
            || self.panel.is_some_and(|panel| panel != Panel::Range)
        {
            return;
        }
        if let Some(info) = &self.info {
            let count = info.row_count.max(1).to_string();
            self.offset.update(cx, |input, cx| input.set_text("0", cx));
            self.count
                .update(cx, |input, cx| input.set_text(&count, cx));
            self.request.load_to_end = self.csv_options.is_some();
            self.run_load_range(cx);
        }
    }
    fn clear_filter(&mut self, cx: &mut Context<Self>) {
        if self.selecting_initial_fields || self.csv_browse.is_some() {
            return;
        }
        self.visual_filters = FilterGroup::default();
        self.clear_filter_draft(cx);
        self.execute(cx);
    }
    fn apply_load_range(&mut self, cx: &mut Context<Self>) {
        self.request.load_to_end = false;
        self.run_load_range(cx);
    }
    fn run_load_range(&mut self, cx: &mut Context<Self>) {
        self.use_loaded_filter_scope(cx);
        self.execute(cx);
        if self.busy.is_some() || self.error.is_none() {
            self.panel = None;
            cx.notify();
        } else if self.result.is_some() && self.panel != Some(Panel::Range) {
            self.restore_displayed_range(cx);
        }
    }

    fn restore_displayed_range(&mut self, cx: &mut Context<Self>) {
        self.request = self.displayed_request.clone();
        let offset = self.request.offset.to_string();
        let count = self.request.limit.to_string();
        self.offset
            .update(cx, |input, cx| input.set_text(&offset, cx));
        self.count
            .update(cx, |input, cx| input.set_text(&count, cx));
    }
    fn awaiting_initial_range(&self) -> bool {
        self.result.is_none()
            && self.info.as_ref().is_some_and(|info| {
                needs_parquet_startup_prompt(self.csv_options.as_ref(), info.file_size)
            })
    }

    fn choose_initial_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel = None;
        self.selecting_initial_fields = true;
        self.open_fields_sidebar(window, cx);
    }

    fn dismiss_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.browse_command = None;
        self.sort_controls = None;
        if self.panel == Some(Panel::CsvImport) {
            self.dismiss_csv_import(cx);
        } else if self.panel == Some(Panel::SavedViews) {
            self.cancel(cx);
            self.saved_views = None;
            self.panel = None;
        } else if self.panel == Some(Panel::Range) && self.awaiting_initial_range() {
            self.close_file(cx);
        } else {
            if self.panel == Some(Panel::Range) && self.csv_browse.is_some() {
                self.chosen_fields
                    .clone_from(&self.displayed_request.selected_columns);
                self.restore_displayed_range(cx);
            }
            self.panel = None;
        }
        self.focus.focus(window);
        cx.notify();
    }
    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        [
            &self.offset,
            &self.count,
            &self.csv_jump,
            &self.field_search,
            &self.detail,
            &self.profile_chart_limit,
            &self.profile_chart_category_limit,
        ]
        .iter()
        .any(|input| input.focus_handle(cx).is_focused(window))
            || self.filter_editor.as_ref().is_some_and(|editor| {
                self.filter_sidebar_open && editor.read(cx).input_focused(window, cx)
            })
            || self.aggregation_editor.as_ref().is_some_and(|editor| {
                self.statistics_tab == Some(StatisticsTab::Aggregation)
                    && editor.read(cx).input_focused(window, cx)
            })
            || self.saved_view_input_focused(window, cx)
            || self.sort_input_focused(window, cx)
            || self.csv_import_input_focused(window, cx)
    }
    fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        self.update_preferences(PreferenceUpdate::Language(language), cx);
    }
    fn show_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        if self.panel.is_some() || self.busy.is_some() || self.selecting_initial_fields {
            return;
        }
        if self.csv_browse.is_some() {
            if panel == Panel::Export {
                self.choose_browse_range(BrowseCommand::Export, cx);
                return;
            }
            if panel == Panel::Range {
                self.suggest_browse_range(cx);
            }
        }
        self.selection_menu = None;
        self.settings_menu = None;
        self.file_menu_open = false;
        if matches!(
            panel,
            Panel::Export | Panel::AggregateExport | Panel::Cell | Panel::Range
        ) && self.info.is_none()
        {
            return;
        }
        self.error = None;
        self.preview_image = None;
        if panel == Panel::ProfileChartLimit {
            let rows = settings_menu::limit_text(self.preferences.profile_chart_max_rows);
            let bars = settings_menu::limit_text(self.preferences.profile_chart_category_bars);
            self.profile_chart_limit
                .update(cx, |input, cx| input.set_text(&rows, cx));
            self.profile_chart_category_limit
                .update(cx, |input, cx| input.set_text(&bars, cx));
        }
        self.panel = Some(panel);
        cx.notify();
    }
    fn copy_selection(&mut self, headers: bool, cx: &mut Context<Self>) {
        self.selection_menu = None;
        self.axis_menu = None;
        if self.csv_browse.is_some() {
            self.copy_csv_browse_selection(headers, cx);
            return;
        }
        let Some(result) = &self.result else {
            return;
        };
        let rows = self.selected_source_rows();
        let columns = self.selected_source_columns();
        if rows.is_empty() || columns.is_empty() {
            return;
        }
        let mut lines = Vec::new();
        if headers {
            lines.push(
                columns
                    .iter()
                    .map(|&column| tsv(&result.columns[column].name))
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
        }
        for row in rows {
            lines.push(
                columns
                    .iter()
                    .map(|&column| {
                        if result.is_null(row, column) {
                            String::new()
                        } else {
                            tsv(&self.cell_text(row, column, false))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
        }
        cx.write_to_clipboard(ClipboardItem::new_string(lines.join("\n")));
        self.notice = "Copied to clipboard".into();
        cx.notify();
    }
    fn show_selection_menu(
        &mut self,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.panel.is_some() || self.busy.is_some() {
            return;
        }
        match self.selection_kind {
            SelectionKind::Rows => {
                self.show_axis_menu(Axis::Rows, row, window, cx);
                return;
            }
            SelectionKind::Columns => {
                self.show_axis_menu(Axis::Columns, col, window, cx);
                return;
            }
            SelectionKind::Cells => {}
        }
        self.axis_menu = None;
        let source_row = self.source_row(row);
        let source_col = self.source_column(col);
        let inside = self.selection.is_some_and(|(start, end)| {
            (start.0.min(end.0)..=start.0.max(end.0)).contains(&row)
                && (start.1.min(end.1)..=start.1.max(end.1)).contains(&col)
        });
        if !inside {
            self.selection = Some(((row, col), (row, col)));
        }
        if self.selection.is_some_and(|(start, end)| start == end)
            && self.audio_cell != Some((source_row, source_col))
        {
            let bytes = self
                .result
                .as_ref()
                .and_then(|result| result.binary(source_row, source_col).map(<[u8]>::to_vec))
                .filter(|bytes| is_audio(bytes));
            if let Some(bytes) = bytes {
                match AudioPlayer::from_bytes(bytes) {
                    Ok(audio) => {
                        self.audio = Some(audio);
                        self.audio_cell = Some((source_row, source_col));
                    }
                    Err(error) => self.fail(error, cx),
                }
            }
        }
        self.selecting = false;
        self.focus.focus(window);
        self.selection_menu = Some(window.mouse_position());
        cx.notify();
    }

    fn render_selection_menu(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(position) = self.selection_menu else {
            return div().into_any_element();
        };
        let c = Colors::new(self.preferences.dark);
        let audio_cell = self.selection.and_then(|(start, end)| {
            let row = self.source_row(start.0);
            let col = self.source_column(start.1);
            (start == end
                && self
                    .result
                    .as_ref()
                    .and_then(|r| r.binary(row, col))
                    .is_some_and(is_audio))
            .then_some((row, col))
        });
        let width = 250.;
        let height = if audio_cell.is_some() { 210. } else { 72. };
        let left =
            f32::from(position.x).min((f32::from(window.viewport_size().width) - width).max(0.));
        let top =
            f32::from(position.y).min((f32::from(window.viewport_size().height) - height).max(0.));
        div()
            .id("selection-menu-backdrop")
            .occlude()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.selection_menu = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.selection_menu = None;
                    cx.notify();
                }),
            )
            .child(
                controls::menu_surface("selection-menu", c)
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(width))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(
                        [("Copy", "⌘C", false), ("Copy with Headers", "⇧⌘C", true)]
                            .into_iter()
                            .enumerate()
                            .map(|(index, (label, key, headers))| {
                                controls::menu_item(("selection-menu-item", index), c, false, true)
                                    .justify_between()
                                    .child(t(label))
                                    .child(div().text_color(rgb(c.muted)).child(key))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_selection(headers, cx)
                                    }))
                            }),
                    )
                    .when_some(audio_cell, |menu, (row, col)| {
                        menu.child(div().h(px(1.)).bg(rgb(c.border)))
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        self.button(
                                            "context-audio-play",
                                            if self
                                                .audio
                                                .as_ref()
                                                .is_some_and(|audio| audio.is_playing())
                                            {
                                                "Pause"
                                            } else {
                                                "Play"
                                            },
                                            true,
                                            move |this, _, cx| {
                                                this.selection_menu = None;
                                                this.play_audio(row, col, cx);
                                            },
                                            cx,
                                        ),
                                    )
                                    .child(self.button(
                                        "context-audio-stop",
                                        "Stop",
                                        true,
                                        |this, _, cx| {
                                            this.selection_menu = None;
                                            if let Some(audio) = &this.audio {
                                                audio.stop();
                                            }
                                            cx.notify();
                                        },
                                        cx,
                                    )),
                            )
                            .child(div().px_2().py_1().child(self.button(
                                "context-audio-save",
                                "Save Value…",
                                true,
                                |this, _, cx| {
                                    this.selection_menu = None;
                                    this.save_cell(cx);
                                },
                                cx,
                            )))
                            .when_some(self.audio.as_ref(), |menu, audio| {
                                menu.child(
                                    div()
                                        .px_3()
                                        .py_2()
                                        .text_color(rgb(c.muted))
                                        .child(t(audio.format_description())),
                                )
                            })
                    }),
            )
            .into_any_element()
    }

    /// Result indices remain local for selection/freezing. Only displayed labels
    /// use the source's stable, one-based row number carried through filtering.
    fn row_number(&self, row: usize) -> usize {
        self.result
            .as_ref()
            .and_then(|result| result.source_row_number(row))
            .unwrap_or_else(|| {
                self.displayed_request
                    .offset
                    .saturating_add(row)
                    .saturating_add(1)
            })
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(result) = &self.result {
            if !result.is_empty() && !result.columns.is_empty() {
                self.selection = Some((
                    (0, 0),
                    (self.table_row_count() - 1, result.columns.len() - 1),
                ));
                self.selection_kind = SelectionKind::Cells;
                cx.notify();
            }
        }
    }
    fn inspect(&mut self, cx: &mut Context<Self>) {
        if self.csv_browse.is_some() {
            self.inspect_csv_browse_selection(cx);
            return;
        }
        if let Some((_, (row, column))) = self.selection {
            self.open_peek(self.source_row(row), self.source_column(column), false, cx);
        }
    }

    pub(super) fn close_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_request = self.preview_request.wrapping_add(1);
        if let Some(cancel) = self.preview_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.preview_pending = None;
        self.preview_subscription = None;
        self.preview_key = None;
        self.preview = None;
        cx.notify();
    }

    pub(super) fn right_sidebar_width(&self, window: &Window) -> f32 {
        if self.preview.is_none()
            && self.preview_pending.is_none()
            && self.statistics_tab.is_none()
            && !self.filter_sidebar_open
            && !self.fields_sidebar_open
        {
            return 0.;
        }
        let viewport = f32::from(window.viewport_size().width);
        if self.statistics_tab.is_some() {
            sidebar_width(
                viewport,
                self.statistics_width
                    .unwrap_or_else(|| (viewport * 0.36).clamp(350., 490.)),
            )
        } else {
            sidebar_width(viewport, self.sidebar_width.unwrap_or(360.))
        }
    }

    fn render_right_sidebar(&mut self, width: f32, cx: &mut Context<Self>) -> gpui::AnyElement {
        let c = Colors::new(self.preferences.dark);
        let content = if self.fields_sidebar_open {
            self.render_fields_sidebar(cx)
        } else if self.filter_sidebar_open {
            self.render_filter_sidebar(cx)
        } else if self.statistics_tab.is_some() {
            self.render_statistics_sidebar(width, cx)
        } else {
            self.render_preview_sidebar(cx)
        };
        div()
            .relative()
            .w(px(width))
            .h_full()
            .min_h_0()
            .flex_shrink_0()
            .child(content)
            .child(
                div()
                    .id("right-sidebar-resize")
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(5.))
                    .cursor_col_resize()
                    .hover(|el| el.bg(rgb(c.accent)))
                    .when(self.sidebar_resize.is_some(), |el| el.bg(rgb(c.accent)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.sidebar_resize = Some(SidebarResize {
                                start: event.position.x,
                                width,
                                statistics: this.statistics_tab.is_some(),
                                dragged: false,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .into_any_element()
    }

    fn render_preview_sidebar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = Colors::new(self.preferences.dark);
        let body = div()
            .id("preview-sidebar")
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(colors.bar))
            .border_l_1()
            .border_color(rgb(colors.border));
        if let Some(preview) = &self.preview {
            body.child(preview.clone()).into_any_element()
        } else {
            body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(colors.border))
                    .child(icons::glyph(Icon::Table, 16., self.preferences.dark))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t("Quick Peek")),
                    )
                    .child(
                        controls::icon_button("preview-close-pending", colors, true)
                            .child(icons::glyph(Icon::Close, 14., self.preferences.dark))
                            .tooltip(icons::tooltip(t("Close"), self.preferences.dark))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_preview(cx);
                                this.focus.focus(window);
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .text_color(rgb(colors.muted))
                    .whitespace_normal()
                    .child(t("Preparing preview…")),
            )
            .into_any_element()
        }
    }

    fn focus_preview(&self, cx: &mut Context<Self>) {
        let (Some(handle), Some(key), Some(preview)) = (
            self.main_window,
            self.preview_key.clone(),
            self.preview.clone(),
        ) else {
            return;
        };
        let request = self.preview_request;
        cx.defer(move |cx| {
            let _ = handle.update(cx, |this, window, cx| {
                if this.preview_request == request && this.preview_key.as_ref() == Some(&key) {
                    preview.update(cx, |preview, cx| preview.activate(window, cx));
                }
            });
        });
    }

    fn open_peek(&mut self, row: usize, column: usize, image_only: bool, cx: &mut Context<Self>) {
        let (Some(result), Some(engine)) = (self.result.clone(), self.engine.as_ref()) else {
            return;
        };
        let Some(metadata) = result
            .columns
            .get(column)
            .filter(|_| row < result.row_count())
        else {
            return;
        };
        let key = PeekKey {
            engine_id: Arc::as_ptr(engine) as usize,
            snapshot: result.snapshot_id.clone(),
            row,
            column: metadata.name.clone(),
            image_only,
        };
        self.close_statistics_sidebar(cx);
        self.close_fields_sidebar();
        if self.preview_key.as_ref() == Some(&key) && self.preview.is_some() {
            self.filter_sidebar_open = false;
            self.focus_preview(cx);
            return;
        }
        if self.preview_pending.as_ref() == Some(&key) {
            return;
        }
        // Dropping the previous entity promptly releases its image/audio/value
        // buffers. Hidden or replaced work is invalidated independently of the
        // query generation, including repeated clicks within the same snapshot.
        let data_type = metadata.data_type.clone();
        let title = format!(
            "{} · Row {} · Quick Peek",
            metadata.name,
            self.row_number(row)
        );
        self.close_preview(cx);
        self.filter_sidebar_open = false;
        self.preview_pending = Some(key.clone());
        let request = self.preview_request;
        let cancel = Arc::new(AtomicBool::new(false));
        self.preview_cancel = Some(cancel.clone());
        let svg_renderer = cx.svg_renderer();
        let task = cx.background_executor().spawn(async move {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut cell = result.preview_cell(row, column, image_only);
            drop(result);
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let nested = cell
                .json
                .take()
                .or_else(|| {
                    (cell.is_null && crate::nested::SqlType::parse(&data_type).is_nested())
                        .then_some(serde_json::Value::Null)
                })
                .map(|value| crate::nested::NestedTable::new(value, &data_type, title));
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let preview = nested
                .as_ref()
                .map(crate::nested::NestedTable::node_json_preview);
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            // Keep the decoded pixels owned by the preview. GPUI's asset
            // cache otherwise retains each distinct data image for the app.
            let image = cell.bytes.as_ref().and_then(|bytes| {
                let format = image_format(bytes)?;
                match Image::from_bytes(format, bytes.clone()).to_image_data(svg_renderer) {
                    Ok(image) => Some(image),
                    Err(error) => {
                        log::warn!("Could not decode cell image: {error:#}");
                        None
                    }
                }
            });
            (!cancel.load(Ordering::Relaxed)).then_some(PreparedPeek {
                cell,
                nested,
                preview,
                image,
            })
        });
        cx.spawn(async move |weak, cx| {
            let prepared = task.await;
            let _ = weak.update(cx, |this, cx| {
                if !crate::peek::pending_matches(
                    request,
                    this.preview_request,
                    &key,
                    this.preview_pending.as_ref(),
                ) {
                    return;
                }
                let current = this
                    .engine
                    .as_ref()
                    .is_some_and(|engine| Arc::as_ptr(engine) as usize == key.engine_id)
                    && this
                        .result
                        .as_ref()
                        .is_some_and(|result| result.snapshot_id == key.snapshot);
                if current && let Some(prepared) = prepared {
                    this.open_prepared_peek(row, column, image_only, prepared, cx);
                } else {
                    this.close_preview(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn open_prepared_peek(
        &mut self,
        row: usize,
        column: usize,
        image_only: bool,
        prepared: PreparedPeek,
        cx: &mut Context<Self>,
    ) {
        let PreparedPeek {
            mut cell,
            nested,
            preview,
            image,
        } = prepared;
        let (Some(result), Some(engine)) = (self.result.clone(), self.engine.as_ref()) else {
            return;
        };
        let Some(metadata) = result.columns.get(column) else {
            return;
        };
        let key = PeekKey {
            engine_id: Arc::as_ptr(engine) as usize,
            snapshot: result.snapshot_id.clone(),
            row,
            column: metadata.name.clone(),
            image_only,
        };
        let text = if let Some(preview) = preview {
            preview
        } else if image_only {
            String::new()
        } else if cell.is_null {
            "NULL".into()
        } else if metadata.data_type == "BLOB" {
            self.cell_text(row, column, true)
        } else {
            pretty_json(&cell.text)
        };
        let title = format!(
            "{} · Row {} · Quick Peek",
            metadata.name,
            self.row_number(row)
        );
        let copy_text = if self.csv_browse.is_some() {
            cell.text.clone()
        } else if image_only || nested.is_some() {
            std::mem::take(&mut cell.text)
        } else {
            self.cell_text(row, column, false)
        };
        let data = PeekData {
            prepared_nested: nested,
            image,
            copy_binary_format: (metadata.data_type == "BLOB").then(|| {
                self.formats
                    .get(&metadata.name)
                    .cloned()
                    .unwrap_or_default()
                    .binary
            }),
            copy_source: image_only.then(|| result.slice_cell(row, column)).flatten(),
            title: title.clone(),
            data_type: metadata.data_type.clone(),
            text,
            copy_text,
            cell,
            sources: self
                .info
                .as_ref()
                .map(|info| info.paths.clone())
                .unwrap_or_default(),
            dark: self.preferences.dark,
            image_only,
        };
        let request = self.preview_request;
        let event_key = key.clone();
        let preview = cx.new(|cx| PeekView::new(data, cx));
        let subscription = cx.subscribe(&preview, move |this, _, _: &PeekEvent, cx| {
            if this.preview_request != request || this.preview_key.as_ref() != Some(&event_key) {
                return;
            }
            let Some(handle) = this.main_window else {
                this.close_preview(cx);
                return;
            };
            let key = event_key.clone();
            // The event originates in this same GPUI window. Defer the window
            // update until dispatch has released its current borrow.
            cx.defer(move |cx| {
                let _ = handle.update(cx, |this, window, cx| {
                    if this.preview_request != request || this.preview_key.as_ref() != Some(&key) {
                        return;
                    }
                    this.close_preview(cx);
                    this.focus.focus(window);
                    cx.notify();
                });
            });
        });
        self.preview_pending = None;
        self.preview_cancel = None;
        self.preview_key = Some(key);
        self.preview_subscription = Some(subscription);
        self.preview = Some(preview);
        self.filter_sidebar_open = false;
        self.focus_preview(cx);
        cx.notify();
    }

    #[allow(dead_code)] // Retain the legacy modal inspector's media helpers.
    fn inspect_modal(&mut self, cx: &mut Context<Self>) {
        let (Some(result), Some((_, (row, col)))) = (&self.result, self.selection) else {
            return;
        };
        let row = self.source_row(row);
        let col = self.source_column(col);
        let cell = result.cell(row, col);
        self.nested = cell
            .json
            .as_ref()
            .map(|value| vec![(result.columns[col].name.clone(), value.clone())])
            .unwrap_or_default();
        let text = if cell.is_null {
            "NULL".into()
        } else if result.columns[col].data_type == "BLOB" {
            let value = self.cell_text(row, col, true);
            if let Some(bytes) = cell.bytes.as_ref().filter(|bytes| bytes.len() > 1024) {
                crate::peek::binary_preview_text(&value, bytes.len())
            } else {
                value
            }
        } else {
            pretty_json(&cell.text)
        };
        self.detail
            .update(cx, |input, cx| input.set_text(&text, cx));
        self.preview_image = cell.bytes.as_ref().and_then(|bytes| {
            image_format(bytes).map(|format| Arc::new(Image::from_bytes(format, bytes.clone())))
        });
        if cell.bytes.as_ref().is_some_and(|bytes| is_audio(bytes))
            && self.audio_cell != Some((row, col))
        {
            match AudioPlayer::from_bytes(cell.bytes.as_ref().unwrap().clone()) {
                Ok(audio) => {
                    self.audio = Some(audio);
                    self.audio_cell = Some((row, col));
                }
                Err(error) => {
                    self.error = Some(error.to_string());
                }
            }
        }
        self.panel = Some(Panel::Cell);
        cx.notify();
    }

    fn play_audio(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        if !self.prepare_audio_cell(row, col, cx) {
            return;
        }
        let audio = self.audio.as_ref().unwrap();
        if audio.is_playing() {
            audio.pause();
        } else if let Err(error) = audio.play() {
            self.fail(error, cx);
            return;
        }
        cx.notify();
    }

    fn prepare_audio_cell(&mut self, row: usize, col: usize, cx: &mut Context<Self>) -> bool {
        if self.audio_cell != Some((row, col)) {
            let Some(bytes) = self
                .result
                .as_ref()
                .and_then(|result| result.binary(row, col).map(<[u8]>::to_vec))
            else {
                return false;
            };
            match AudioPlayer::from_bytes(bytes) {
                Ok(audio) => {
                    self.audio = Some(audio);
                    self.audio_cell = Some((row, col));
                }
                Err(error) => {
                    self.fail(error, cx);
                    return false;
                }
            }
        }
        self.audio.is_some()
    }

    fn inline_audio_controls(
        &self,
        row: usize,
        col: usize,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let c = Colors::new(self.preferences.dark);
        let current = self.audio_cell == Some((row, col));
        let audio = self.audio.as_ref().filter(|_| current);
        let playing = audio.is_some_and(|audio| audio.is_playing());
        let position = audio.map_or(0., |audio| audio.position());
        let duration = audio.map_or(0., |audio| audio.duration());
        let weak = cx.entity().downgrade();
        let cell_id = self
            .result
            .as_ref()
            .map_or(col, |result| row * result.columns.len() + col);
        div()
            .id(("inline-audio", cell_id))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.focus.focus(window);
                    if let (Some(vr), Some(vc)) = (this.view_row(row), this.view_column(col)) {
                        this.selection = Some(((vr, vc), (vr, vc)));
                        this.selection_kind = SelectionKind::Cells;
                    }
                    if event.click_count == 2 {
                        this.open_peek(row, col, false, cx);
                    }
                    cx.notify();
                }),
            )
            .child(
                controls::icon_button(("inline-audio-play", cell_id), c, true)
                    .size(px(18.))
                    .child(icons::glyph(
                        if playing { Icon::Pause } else { Icon::Play },
                        14.,
                        self.preferences.dark,
                    ))
                    .tooltip(icons::tooltip(
                        if playing { "Pause audio" } else { "Play audio" },
                        self.preferences.dark,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| this.play_audio(row, col, cx))),
            )
            .child(
                controls::icon_button(("inline-audio-stop", cell_id), c, current)
                    .size(px(18.))
                    .child(icons::glyph(Icon::Stop, 14., self.preferences.dark))
                    .tooltip(icons::tooltip("Stop audio", self.preferences.dark))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.audio_cell == Some((row, col)) {
                            if let Some(audio) = &this.audio {
                                audio.stop();
                            }
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id(("inline-audio-track", cell_id))
                    .flex_1()
                    .min_w(px(24.))
                    .h(px(12.))
                    .cursor_pointer()
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                window.paint_quad(gpui::fill(bounds, rgb(c.border)));
                                if duration > 0. {
                                    window.paint_quad(gpui::fill(
                                        Bounds::new(
                                            bounds.origin,
                                            size(
                                                bounds.size.width * (position / duration) as f32,
                                                bounds.size.height,
                                            ),
                                        ),
                                        rgb(c.accent),
                                    ));
                                }
                                let target = weak.clone();
                                window.on_mouse_event(
                                    move |event: &MouseDownEvent, phase, window, cx| {
                                        if phase != gpui::DispatchPhase::Bubble
                                            || event.button != MouseButton::Left
                                            || !bounds.contains(&event.position)
                                            || bounds.size.width <= px(0.)
                                        {
                                            return;
                                        }
                                        if let Some(view) = target.upgrade() {
                                            view.update(cx, |this, cx| {
                                                if this.panel.is_some()
                                                    || this.busy.is_some()
                                                    || this.selection_menu.is_some()
                                                {
                                                    return;
                                                }
                                                cx.stop_propagation();
                                                this.focus.focus(window);
                                                if let (Some(vr), Some(vc)) =
                                                    (this.view_row(row), this.view_column(col))
                                                {
                                                    this.selection = Some(((vr, vc), (vr, vc)));
                                                    this.selection_kind = SelectionKind::Cells;
                                                }
                                                if event.click_count == 2 {
                                                    this.open_peek(row, col, false, cx);
                                                    return;
                                                }
                                                if this.prepare_audio_cell(row, col, cx) {
                                                    if let Some(audio) = &this.audio {
                                                        audio.seek(
                                                            f64::from(
                                                                ((event.position.x
                                                                    - bounds.left())
                                                                    / bounds.size.width)
                                                                    .clamp(0., 1.),
                                                            ) * audio.duration(),
                                                        );
                                                    }
                                                    this.audio_seek_drag = true;
                                                }
                                                cx.notify();
                                            });
                                        }
                                    },
                                );
                                let target = weak.clone();
                                window.on_mouse_event(
                                    move |event: &MouseMoveEvent, phase, _, cx| {
                                        if phase != gpui::DispatchPhase::Bubble
                                            || !event.dragging()
                                            || bounds.size.width <= px(0.)
                                        {
                                            return;
                                        }
                                        if let Some(view) = target.upgrade() {
                                            view.update(cx, |this, cx| {
                                                if this.audio_seek_drag
                                                    && this.audio_cell == Some((row, col))
                                                {
                                                    if let Some(audio) = &this.audio {
                                                        audio.seek(
                                                            f64::from(
                                                                ((event.position.x
                                                                    - bounds.left())
                                                                    / bounds.size.width)
                                                                    .clamp(0., 1.),
                                                            ) * audio.duration(),
                                                        );
                                                    }
                                                    cx.notify();
                                                }
                                            });
                                        }
                                    },
                                );
                            },
                        )
                        .size_full(),
                    ),
            )
            .child(
                controls::icon_button(("inline-audio-menu", cell_id), c, true)
                    .size(px(18.))
                    .child("⋯")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let (Some(vr), Some(vc)) = (this.view_row(row), this.view_column(col)) {
                            this.selection_kind = SelectionKind::Cells;
                            this.show_selection_menu(vr, vc, window, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn audio_controls(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(audio) = self.audio.as_ref() else {
            return div().into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.button(
                        "audio-play",
                        if audio.is_playing() { "Pause" } else { "Play" },
                        true,
                        |this, _, cx| {
                            let Some((row, col)) = this.audio_cell else {
                                return;
                            };
                            this.play_audio(row, col, cx);
                        },
                        cx,
                    ))
                    .child(self.button(
                        "audio-stop",
                        "Stop",
                        true,
                        |this, _, cx| {
                            if let Some(audio) = &this.audio {
                                audio.stop();
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(self.button(
                        "audio-back",
                        "−5 s",
                        true,
                        |this, _, cx| {
                            if let Some(audio) = &this.audio {
                                audio.seek(audio.position() - 5.);
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(self.button(
                        "audio-forward",
                        "+5 s",
                        true,
                        |this, _, cx| {
                            if let Some(audio) = &this.audio {
                                audio.seek(audio.position() + 5.);
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(format!(
                        "{:.1} / {:.1} s",
                        audio.position(),
                        audio.duration()
                    )),
            )
            .child(self.audio_seekbar(cx))
            .into_any_element()
    }

    fn audio_seekbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let c = Colors::new(self.preferences.dark);
        let Some(audio) = &self.audio else {
            return div().into_any_element();
        };
        let duration = audio.duration();
        let position = audio.position();
        let weak = cx.entity().downgrade();
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                window.paint_quad(gpui::fill(bounds, rgb(c.border)));
                if duration <= 0. {
                    return;
                }
                window.paint_quad(gpui::fill(
                    Bounds::new(
                        bounds.origin,
                        size(
                            bounds.size.width * (position / duration) as f32,
                            bounds.size.height,
                        ),
                    ),
                    rgb(c.accent),
                ));
                let target = weak.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble
                        || event.button != MouseButton::Left
                        || !bounds.contains(&event.position)
                    {
                        return;
                    }
                    if let Some(entity) = target.upgrade() {
                        entity.update(cx, |this, cx| {
                            if let Some(audio) = &this.audio {
                                audio.seek(
                                    f64::from(
                                        ((event.position.x - bounds.left()) / bounds.size.width)
                                            .clamp(0., 1.),
                                    ) * duration,
                                );
                            }
                            this.audio_seek_drag = true;
                            cx.notify();
                        });
                    }
                });
                let target = weak.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble || !event.dragging() {
                        return;
                    }
                    if let Some(entity) = target.upgrade() {
                        entity.update(cx, |this, cx| {
                            if this.audio_seek_drag {
                                if let Some(audio) = &this.audio {
                                    audio.seek(
                                        f64::from(
                                            ((event.position.x - bounds.left())
                                                / bounds.size.width)
                                                .clamp(0., 1.),
                                        ) * duration,
                                    );
                                }
                                cx.notify();
                            }
                        });
                    }
                });
            },
        )
        .h(px(10.))
        .w_full()
        .into_any_element()
    }

    fn prepare_binary_formats(&mut self, column: usize, cx: &mut Context<Self>) {
        let Some(result) = self.result.clone().filter(|result| {
            result
                .columns
                .get(column)
                .is_some_and(|c| c.data_type == "BLOB")
        }) else {
            return;
        };
        if self.binary_capabilities.contains_key(&column)
            || !self.binary_capabilities_pending.insert(column)
        {
            return;
        }
        let view = Arc::downgrade(&result);
        let cancel = self.binary_scan_cancel.clone();
        let worker_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            let mut available = BinaryFormat::all().to_vec();
            for row in 0..result.row_count() {
                if worker_cancel.load(Ordering::Relaxed) {
                    return None;
                }
                if let Some(bytes) = result.binary(row, column) {
                    available.retain(|format| format.is_supported(bytes));
                }
            }
            Some(available)
        });
        cx.spawn(async move |weak, cx| {
            let Some(available) = task.await else { return };
            let _ = weak.update(cx, |this, cx| {
                if !cancel.load(Ordering::Relaxed)
                    && this
                        .result
                        .as_ref()
                        .is_some_and(|result| view.ptr_eq(&Arc::downgrade(result)))
                {
                    this.binary_capabilities_pending.remove(&column);
                    this.binary_capabilities.insert(column, available);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn cell_text(&self, row: usize, col: usize, clipped: bool) -> String {
        let Some(result) = &self.result else {
            return String::new();
        };
        if result.is_null(row, col) {
            return "NULL".into();
        }
        let column = &result.columns[col];
        let style = self.formats.get(&column.name).cloned().unwrap_or_default();
        if column.data_type == "BLOB" {
            if let Some(bytes) = result.binary(row, col) {
                let bytes = if clipped
                    && matches!(
                        style.binary,
                        BinaryFormat::Hex | BinaryFormat::Base64 | BinaryFormat::Ascii
                    ) {
                    &bytes[..bytes.len().min(1024)]
                } else {
                    bytes
                };
                return style
                    .binary
                    .apply(bytes)
                    .unwrap_or_else(|| result.display_text(row, col));
            }
        }
        // Full materialization belongs to explicit copy/inspect actions only.
        let text = if clipped {
            result.display_text(row, col)
        } else {
            result.cell(row, col).text
        };
        style.float.apply(&text, &column.data_type).unwrap_or(text)
    }

    fn render_nested(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some((_, value)) = self.nested.last() else {
            return div().into_any_element();
        };
        let colors = Colors::new(self.preferences.dark);
        let children: Vec<(String, serde_json::Value)> = match value {
            serde_json::Value::Object(object) => object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            serde_json::Value::Array(array) => array
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value.clone()))
                .collect(),
            _ => Vec::new(),
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(self.button(
                        "nested-back",
                        "‹ Back",
                        self.nested.len() > 1,
                        |this, _, cx| {
                            this.nested.pop();
                            if let Some((_, value)) = this.nested.last() {
                                let text = serde_json::to_string_pretty(value)
                                    .unwrap_or_else(|_| value.to_string());
                                this.detail
                                    .update(cx, |input, cx| input.set_text(&text, cx));
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(
                        self.nested
                            .iter()
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>()
                            .join(" / "),
                    ),
            )
            .child(
                div()
                    .id("nested-table")
                    .flex_1()
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(rgb(colors.border))
                    .children(
                        children
                            .into_iter()
                            .enumerate()
                            .map(|(index, (key, value))| {
                                let expandable = value.is_object() || value.is_array();
                                let text = if let Some(text) = value.as_str() {
                                    text.to_owned()
                                } else {
                                    value.to_string()
                                };
                                div()
                                    .id(("nested-row", index))
                                    .flex()
                                    .border_b_1()
                                    .border_color(rgb(colors.border))
                                    .p_2()
                                    .gap_3()
                                    .child(div().w(px(150.)).flex_shrink_0().child(key.clone()))
                                    .child(
                                        div()
                                            .flex_1()
                                            .truncate()
                                            .child(text.chars().take(2000).collect::<String>()),
                                    )
                                    .when(expandable, |el| {
                                        el.cursor_pointer()
                                            .child(t("Open ›"))
                                            .hover(|el| el.bg(rgb(colors.selected)))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if expandable {
                                            this.nested.push((key.clone(), value.clone()));
                                        }
                                        this.detail.update(cx, |input, cx| {
                                            input.set_text(&pretty_json(&text), cx)
                                        });
                                        cx.notify();
                                    }))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn save_cell(&mut self, cx: &mut Context<Self>) {
        let (Some(result), Some((_, (row, col)))) = (&self.result, self.selection) else {
            return;
        };
        let row = self.source_row(row);
        let col = self.source_column(col);
        let cell = result.cell(row, col);
        let data = cell
            .bytes
            .clone()
            .unwrap_or_else(|| cell.text.as_bytes().to_vec());
        let extension = cell
            .bytes
            .as_ref()
            .map(|bytes| binary_extension(bytes))
            .unwrap_or("txt");
        let response = cx.prompt_for_new_path(
            std::path::Path::new("/tmp"),
            Some(&format!("cell.{extension}")),
        );
        cx.spawn(async move |weak, cx| {
            if let Ok(Ok(Some(path))) = response.await {
                let sources = weak
                    .read_with(cx, |this, _| {
                        this.info
                            .as_ref()
                            .map(|info| info.paths.clone())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                let task = cx
                    .background_executor()
                    .spawn(async move { crate::source_guard::save_copy(&path, &sources, &data) });
                let result = task.await;
                let _ = weak.update(cx, |this, cx| match result {
                    Ok(()) => {
                        this.notice = "Cell saved".into();
                        cx.notify();
                    }
                    Err(e) => this.fail(e, cx),
                });
            }
        })
        .detach();
    }
    fn preview_external(&mut self, cx: &mut Context<Self>) {
        let (Some(result), Some((_, (row, col)))) = (&self.result, self.selection) else {
            return;
        };
        let row = self.source_row(row);
        let col = self.source_column(col);
        let Some(bytes) = result.binary(row, col) else {
            return;
        };
        let directory = std::env::temp_dir().join(format!("parquetviewer-{}", std::process::id()));
        let path = directory.join(format!("cell-{row}-{col}.{}", binary_extension(bytes)));
        match std::fs::create_dir_all(directory)
            .and_then(|_| crate::source_guard::save_copy(&path, &[], bytes))
        {
            Ok(()) => cx.open_with_system(&path),
            Err(error) => self.fail(error, cx),
        }
    }

    fn button(
        &self,
        id: &'static str,
        label: impl Into<String>,
        enabled: bool,
        handler: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.preferences.dark);
        let label = label.into();
        let primary = matches!(
            id,
            "empty-open"
                | "execute"
                | "apply-fields"
                | "import-load"
                | "csv-import"
                | "apply-profile-chart-limit"
        );
        let style = if primary {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Standard
        };
        controls::button(id, c, style, enabled)
            .when(enabled, |el| {
                el.on_click(cx.listener(move |this, _, window, cx| {
                    this.focus.focus(window);
                    handler(this, window, cx)
                }))
            })
            .children(Icon::for_command(id).map(|icon| {
                if primary {
                    icons::primary_glyph(icon, 14.)
                } else {
                    icons::glyph(icon, 14., self.preferences.dark)
                }
            }))
            .tooltip(icons::tooltip(t(label.clone()), self.preferences.dark))
            .child(t(label))
    }
    fn input_shell(&self, input: &Entity<TextInput>, width: Option<f32>) -> gpui::Div {
        // TextInput owns its border, background, padding and focus state.
        // This wrapper only participates in the surrounding row's layout.
        div()
            .h(px(28.))
            .min_w(px(30.))
            .when_some(width, |el, width| el.w(px(width)).flex_shrink_0())
            .when(width.is_none(), |el| el.flex_1())
            .child(input.clone())
    }

    fn replace_loaded_result(&mut self, result: Option<Arc<QueryResult>>, cx: &mut Context<Self>) {
        if result.is_none() {
            self.close_statistics_sidebar(cx);
        }
        if let Some(next) = &result
            && (self.widths.len() != next.columns.len()
                || self.result.as_ref().is_none_or(|previous| {
                    !previous
                        .columns
                        .iter()
                        .map(|column| column.name.as_str())
                        .eq(next.columns.iter().map(|column| column.name.as_str()))
                }))
        {
            let previous_names = self.result.as_ref().map_or_else(Vec::new, |result| {
                result
                    .columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>()
            });
            let next_names = next
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>();
            // Keep the old result and geometry valid while queries are pending.
            // Publish the new field-aligned widths and result in this update.
            self.widths = crate::table_state::reconcile_column_widths(
                &previous_names,
                &self.widths,
                &next_names,
            );
        }
        let fixed = result.as_ref().is_none_or(|result| {
            !result.columns.iter().any(|column| {
                self.formats
                    .get(&column.name)
                    .is_some_and(|style| style.word_wrap)
            })
        });
        // A new result starts at the top. Switch storage while empty so going
        // from a huge uniform result to a small wrapped result never builds a
        // temporary measured tree for the old row count.
        if self.scroll.main.fixed_item_height().is_some() != fixed
            || self.pinned_scroll.main.fixed_item_height().is_some() != fixed
        {
            self.scroll.reset(0);
            self.pinned_scroll.reset(0);
            self.scroll.set_fixed_height(fixed);
            self.pinned_scroll.set_fixed_height(fixed);
        }
        if let Some(editor) = &self.filter_editor {
            let loaded_result = result.as_ref().map(Arc::downgrade).unwrap_or_default();
            editor.update(cx, |editor, _| editor.set_loaded_result(loaded_result));
        }
        if let Some(previous) = std::mem::replace(&mut self.result, result) {
            cx.background_executor()
                .spawn(async move { drop(previous) })
                .detach();
        }
        self.refresh_column_profile(cx);
    }

    fn replace_engine(&mut self, engine: Option<Arc<Mutex<Engine>>>, cx: &App) {
        if let Some(previous) = std::mem::replace(&mut self.engine, engine) {
            cx.background_executor()
                .spawn(async move { drop(previous) })
                .detach();
        }
    }

    fn reset_table_scroll(&mut self) {
        self.display_cache.borrow_mut().clear();
        self.binary_capabilities.clear();
        self.binary_capabilities_pending.clear();
        self.binary_scan_cancel.store(true, Ordering::Relaxed);
        self.binary_scan_cancel = Arc::new(AtomicBool::new(false));
        // refresh_table_order resets both panes once after publishing the result.
        self.row_layout_key = None;
        self.row_heights.clear();
    }

    fn reveal_column(&self, column: usize, _window: &Window) {
        if column >= self.view_columns.len() {
            return;
        }
        let (handle, left, right) = if column < self.frozen_count {
            let left = 52.
                + self.view_columns[..column]
                    .iter()
                    .map(|&c| self.widths[c])
                    .sum::<f32>();
            (
                &self.frozen_horizontal,
                left,
                left + self.widths[self.source_column(column)],
            )
        } else {
            let left = self.view_columns[self.frozen_count..column]
                .iter()
                .map(|&c| self.widths[c])
                .sum::<f32>();
            (
                &self.horizontal,
                left,
                left + self.widths[self.source_column(column)],
            )
        };
        let offset = handle.offset();
        let visible = -f32::from(offset.x);
        let width = f32::from(handle.bounds().size.width);
        let next = if left < visible {
            left
        } else if right > visible + width {
            (right - width).max(0.)
        } else {
            visible
        };
        handle.set_offset(point(
            -px(next.min(f32::from(handle.max_offset().width))),
            offset.y,
        ));
    }
    fn reveal_row(&self, row: usize) {
        if row < self.frozen_row_count {
            self.pinned_scroll.reveal(row);
        } else {
            self.scroll.reveal(row - self.frozen_row_count);
        }
    }

    /// Scroll the displayed result only: never change the source loading range,
    /// query snapshot, selected cells, or horizontal position.
    fn jump_loaded_rows(&mut self, last: bool, cx: &mut Context<Self>) {
        if self.busy.is_some()
            || self.panel.is_some()
            || self.result.as_ref().is_none_or(|result| result.is_empty())
        {
            return;
        }
        self.settings_menu = None;
        self.file_menu_open = false;
        self.axis_menu = None;
        self.selection_menu = None;
        self.selecting = false;
        if last {
            let rows = if self.scroll.main.item_count() > 0 {
                &self.scroll
            } else {
                &self.pinned_scroll
            };
            rows.set_offset(rows.main.max_offset_for_scrollbar().height);
        } else {
            self.scroll.set_offset(px(0.));
            self.pinned_scroll.set_offset(px(0.));
        }
        cx.notify();
    }

    fn auto_size_column(&mut self, column: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self
            .result
            .as_ref()
            .filter(|result| column < result.columns.len())
        else {
            return;
        };
        let measure = |text: &str| {
            text.lines()
                .map(|line| {
                    let run = TextRun {
                        len: line.len(),
                        font: gpui::font("Helvetica Neue"),
                        color: rgb(0).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    f32::from(
                        window
                            .text_system()
                            .shape_line(line.to_owned().into(), px(12.), &[run], None)
                            .width,
                    )
                })
                .fold(0., f32::max)
        };
        // Leave room for the semibold header, sort direction and pin glyph,
        // including when this column is frozen after auto sizing.
        let mut width = (measure(&result.columns[column].name) + 44.).max(measure("NULL") + 18.);
        for row in column_width_sample_rows(result.row_count()) {
            width = width.max(measure(&self.cell_text(row, column, true)) + 18.);
            if width >= 360. {
                break;
            }
        }
        if let Some(target) = self.widths.get_mut(column) {
            *target = width.clamp(70., 360.);
        }
        self.resize = None;
        cx.notify();
    }

    fn ensure_row_layout(&mut self, result: &QueryResult) {
        if self.csv_browse.is_some() {
            self.wrapped_columns.clear();
            return;
        }
        let key = RowLayoutKey {
            generation: self.generation,
            snapshot: result.snapshot_id.clone(),
            widths: self.widths.clone(),
            formats: self.formats.clone(),
        };
        if self.row_layout_key.as_ref() != Some(&key) {
            let geometry_changed = self
                .row_layout_key
                .as_ref()
                .is_some_and(|previous| key.changes_row_heights(previous, &result.columns));
            if geometry_changed || self.row_layout_key.is_none() {
                self.row_heights.clear();
            }
            if self.row_layout_key.as_ref().is_none_or(|previous| {
                key.snapshot != previous.snapshot || key.formats != previous.formats
            }) {
                self.display_cache.borrow_mut().clear();
            }
            self.wrapped_columns = result
                .columns
                .iter()
                .enumerate()
                .filter(|(_, column)| {
                    self.formats
                        .get(&column.name)
                        .is_some_and(|style| style.word_wrap)
                })
                .map(|(index, _)| index)
                .collect();
            let fixed = self.wrapped_columns.is_empty();
            let mode_changed =
                self.scroll.set_fixed_height(fixed) | self.pinned_scroll.set_fixed_height(fixed);
            if geometry_changed && !mode_changed {
                self.scroll
                    .invalidate(result.row_count().saturating_sub(self.frozen_row_count));
                self.pinned_scroll.invalidate(self.frozen_row_count);
            } else if self.scroll.main.item_count()
                != result.row_count().saturating_sub(self.frozen_row_count)
            {
                self.scroll
                    .reset(result.row_count().saturating_sub(self.frozen_row_count));
                self.pinned_scroll.reset(self.frozen_row_count);
            }
            self.row_layout_key = Some(key);
        }
    }

    fn row_text(&self, row: usize, column: usize, wrapped: bool) -> SharedString {
        let Some(result) = &self.result else {
            return "".into();
        };
        if result.is_null(row, column) {
            return "NULL".into();
        }
        let key = crate::display_cache::Key {
            row,
            column,
            wrapped,
        };
        if let Some(text) = self.display_cache.borrow_mut().get(key) {
            return text;
        }
        // Arrow summaries do not serialize nested values. A long string is
        // clipped before allocation; full values remain available to copy/save.
        let text = self.cell_text(row, column, true);
        let text = if result.is_nested(row, column) {
            t(text)
        } else {
            text
        };
        let text = if wrapped {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.replace('\n', " ↵ ").replace('\r', "")
        };
        self.display_cache
            .borrow_mut()
            .insert(key, text, &self.resources)
    }

    /// Only wrapped columns contribute to row height. Keeping their full set
    /// stable across both panes prevents heights jumping on horizontal scroll.
    fn measured_row_height(&mut self, row: usize, window: &mut Window) -> Pixels {
        if self.wrapped_columns.is_empty() {
            return px(TABLE_ROW_MIN_HEIGHT);
        }
        if let Some(height) = self.row_heights.get(&row) {
            return *height;
        }
        let mut height = px(TABLE_ROW_MIN_HEIGHT);
        if let Some(result) = &self.result {
            let font = window.text_style().font();
            for &column in &self.wrapped_columns {
                if result.binary(row, column).is_some_and(is_audio) {
                    continue;
                }
                // Match the cell's 16px padding + 1px border, and the nested
                // value's 14px icon + 4px gap. Measuring without the icon makes
                // the real text wrap into more lines than its row can hold.
                let decoration = if result.is_nested(row, column) {
                    18.
                } else {
                    0.
                };
                let wrap_width = px((self.widths.get(column).copied().unwrap_or(150.)
                    - 17.
                    - decoration)
                    .max(4.));
                let text = self.row_text(row, column, true);
                let run = TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: rgb(0x000000).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let text_height = window
                    .text_system()
                    .shape_text(text, px(12.), &[run], Some(wrap_width), None)
                    .map(|lines| {
                        lines
                            .iter()
                            .map(|line| line.size(px(TABLE_TEXT_LINE_HEIGHT)).height)
                            .fold(px(0.), |total, height| total + height)
                    })
                    .unwrap_or(px(TABLE_TEXT_LINE_HEIGHT));
                height = height.max(text_height + px(9.));
            }
        }
        // GPUI owns its list geometry; this auxiliary lookup must not retain
        // an unbounded entry for every row ever visited.
        if self.row_heights.len() >= 8192 {
            self.row_heights.clear();
        }
        self.row_heights.insert(row, height);
        height
    }

    fn render_workspace(&mut self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.result.is_none() {
            return self.render_table(window, cx);
        }
        let c = Colors::new(self.preferences.dark);
        div()
            .size_full()
            .bg(rgb(c.surface))
            .child(self.render_table(window, cx))
            .into_any_element()
    }

    fn render_scrollbar(
        &self,
        horizontal: bool,
        content: f32,
        handle: TableScrollbar,
        track_id: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let weak = cx.entity().downgrade();
        let c = Colors::new(self.preferences.dark);
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                window.paint_quad(gpui::fill(bounds, rgb(c.bar)));
                let length = f32::from(if horizontal {
                    bounds.size.width
                } else {
                    bounds.size.height
                });
                let content = match &handle {
                    TableScrollbar::Rows(rows) => {
                        f32::from(rows.main.max_offset_for_scrollbar().height) + length
                    }
                    TableScrollbar::Pixels(_) => content,
                };
                if content <= length || length <= 24. {
                    return;
                }
                let thumb = (length * length / content).max(24.).min(length);
                let offset = -f32::from(if horizontal {
                    handle.offset().x
                } else {
                    handle.offset().y
                });
                let position =
                    (offset / (content - length) * (length - thumb)).clamp(0., length - thumb);
                let rect = if horizontal {
                    Bounds::new(
                        point(bounds.left() + px(position), bounds.top() + px(3.)),
                        size(px(thumb), px(6.)),
                    )
                } else {
                    Bounds::new(
                        point(bounds.left() + px(3.), bounds.top() + px(position)),
                        size(px(6.), px(thumb)),
                    )
                };
                window.paint_quad(gpui::fill(rect, rgb(c.scrollbar)));
                let target = weak.clone();
                let scroll = handle.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                    if event.button != MouseButton::Left
                        || phase != gpui::DispatchPhase::Bubble
                        || !bounds.contains(&event.position)
                    {
                        return;
                    }
                    let Some(entity) = target.upgrade() else {
                        return;
                    };
                    if entity.read(cx).panel.is_some() || entity.read(cx).busy.is_some() {
                        return;
                    }
                    let point = f32::from(if horizontal {
                        event.position.x - bounds.left()
                    } else {
                        event.position.y - bounds.top()
                    });
                    let value = ((point - thumb / 2.) / (length - thumb) * (content - length))
                        .clamp(0., content - length);
                    scroll.drag_started();
                    let old = scroll.offset();
                    scroll.set_offset(if horizontal {
                        gpui::point(-px(value), old.y)
                    } else {
                        gpui::point(old.x, -px(value))
                    });
                    entity.update(cx, |this, cx| {
                        this.scroll_drag = Some(track_id);
                        cx.notify();
                    });
                });
                let target = weak.clone();
                let scroll = handle.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble || !event.dragging() {
                        return;
                    }
                    let Some(entity) = target.upgrade() else {
                        return;
                    };
                    if entity.read(cx).scroll_drag != Some(track_id) {
                        return;
                    }
                    let point = f32::from(if horizontal {
                        event.position.x - bounds.left()
                    } else {
                        event.position.y - bounds.top()
                    });
                    let value = ((point - thumb / 2.) / (length - thumb) * (content - length))
                        .clamp(0., content - length);
                    let old = scroll.offset();
                    scroll.set_offset(if horizontal {
                        gpui::point(-px(value), old.y)
                    } else {
                        gpui::point(old.x, -px(value))
                    });
                    cx.notify(entity.entity_id());
                });
                let scroll = handle.clone();
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, _| {
                    if phase == gpui::DispatchPhase::Capture {
                        scroll.drag_ended();
                    }
                });
            },
        )
        .when(horizontal, |el| el.w_full().h(px(12.)).flex_shrink_0())
        .when(!horizontal, |el| el.size_full())
        .into_any_element()
    }

    fn render_panel(&mut self, toolbar_height: f32, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(panel) = self.panel else {
            return div().into_any_element();
        };
        let c = Colors::new(self.preferences.dark);
        let title = match panel {
            Panel::CsvImport => "Text File Import",
            Panel::Export => "Export Current Results",
            Panel::AggregateExport => "Export Summary Results",
            Panel::Sort => "Sort & Top N",
            Panel::SavedViews => "Saved Views",
            Panel::Cell => "Cell Inspector",
            Panel::Help => "Table & Filter Help",
            Panel::About => "About Parquet Viewer",
            Panel::ColumnDisplay { binary: true, .. } => "Binary Display",
            Panel::ColumnDisplay { binary: false, .. } => "Floating Point Display",
            Panel::ProfileChartLimit => "Chart Data Limits",
            Panel::Range => "Loading range…",
            Panel::CsvJump => "Go to Row",
        };
        let mut body = div()
            .id("panel-body")
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .map(|body| {
                if panel == Panel::CsvImport {
                    body.flex_initial().px(px(16.)).py(px(10.))
                } else {
                    body.flex_1()
                }
            })
            .min_h_0()
            .when(
                matches!(
                    panel,
                    Panel::ColumnDisplay { .. }
                        | Panel::CsvImport
                        | Panel::Range
                        | Panel::ProfileChartLimit
                        | Panel::Export
                        | Panel::AggregateExport
                        | Panel::About
                        | Panel::Sort
                ),
                |body| body.overflow_y_scroll(),
            );
        match panel {
            Panel::CsvImport => body = body.child(self.render_csv_import(cx)),
            Panel::CsvJump => body = body.child(self.render_csv_jump(cx)),
            Panel::Sort => body = body.child(self.render_sort_controls(cx)),
            Panel::SavedViews => body = body.child(self.render_saved_views_body(cx)),
            Panel::Range => {
                let total_rows = self
                    .info
                    .as_ref()
                    .filter(|info| info.row_count_known)
                    .map(|info| info.row_count);
                let remaining_rows = total_rows.and_then(|total| {
                    self.offset
                        .read(cx)
                        .text()
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .map(|offset| total.saturating_sub(offset))
                });
                body = body
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "tab" {
                            let target = if this.offset.focus_handle(cx).is_focused(window) {
                                this.count.focus_handle(cx)
                            } else {
                                this.offset.focus_handle(cx)
                            };
                            target.focus(window);
                            cx.stop_propagation();
                        }
                    }))
                    .child(div().text_color(rgb(c.muted)).whitespace_normal().child(t(
                        if self.browse_command.is_some() {
                            "Choose which source rows to use for this operation."
                        } else if self.awaiting_initial_range() {
                            "This dataset is at least 5 GB. Choose a row range or load all records."
                        } else {
                            "Choose the source row window to load before filtering."
                        },
                    )))
                    .when(total_rows.is_none() && self.csv_options.is_some(), |body| {
                        body.child(div().text_color(rgb(c.muted)).child(t(
                            "Total rows will be available after the text row index reaches the end of the file.",
                        )))
                    })
                    .when_some(total_rows, |body, total| {
                        body.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .py_2()
                                .border_b_1()
                                .border_color(rgb(c.border))
                                .text_size(px(11.))
                                .child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .gap_3()
                                        .child(t("Total source rows"))
                                        .child(count_label(total)),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .gap_3()
                                        .text_color(rgb(c.muted))
                                        .child(t("Rows remaining from offset"))
                                        .child(
                                            remaining_rows
                                                .map(count_label)
                                                .unwrap_or_else(|| "—".into()),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(div().w(px(60.)).child(t("Offset")))
                            .child(self.input_shell(&self.offset, None).flex_1()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(div().w(px(60.)).child(t("Rows")))
                            .child(self.input_shell(&self.count, None).flex_1()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_end()
                            .gap_2()
                            .when(self.awaiting_initial_range() || self.csv_browse.is_some(), |actions| {
                                actions.child(self.button(
                                    "initial-range-fields",
                                    "Choose Fields…",
                                    true,
                                    |this, window, cx| this.choose_initial_fields(window, cx),
                                    cx,
                                ))
                            })
                            .child(self.button(
                                "load-all",
                                "Load All Records",
                                true,
                                |this, _, cx| this.load_all(cx),
                                cx,
                            ))
                            .child(self.button(
                                "apply-load-range",
                                "Load window",
                                true,
                                |this, _, cx| this.apply_load_range(cx),
                                cx,
                            )),
                    );
            }
            Panel::Export | Panel::AggregateExport => {
                body = body.child(self.render_export_formats(panel == Panel::AggregateExport, cx));
            }
            Panel::Cell => {
                if let Some((_, (row, col))) = self.selection {
                    let row = self.source_row(row);
                    let col = self.source_column(col);
                    if self.audio_cell == Some((row, col)) {
                        body = body.child(self.audio_controls(cx));
                    }
                }
                if !self.nested.is_empty() {
                    body = body.child(self.render_nested(cx));
                }
                if let (Some(result), Some((_, (row, col)))) = (&self.result, self.selection) {
                    let row = self.source_row(row);
                    let col = self.source_column(col);
                    body = body.child(t(format!(
                        "Row {} · {} · {}",
                        self.row_number(row),
                        result.columns[col].name,
                        result.columns[col].data_type
                    )));
                }
                if let Some(image) = self.preview_image.clone() {
                    body = body.child(
                        img(image)
                            .max_h(px(300.))
                            .max_w_full()
                            .object_fit(gpui::ObjectFit::Contain),
                    );
                    body = body.child(self.button(
                        "copy-image",
                        "Copy Image",
                        true,
                        |this, _, cx| {
                            if let Some(image) = &this.preview_image {
                                cx.write_to_clipboard(ClipboardItem::from((**image).clone()));
                            }
                        },
                        cx,
                    ));
                }
                body = body
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.detail.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(self.button(
                                "copy-cell",
                                "Copy Value",
                                true,
                                |this, _, cx| {
                                    if this.nested.is_empty() {
                                        this.copy_selection(false, cx);
                                    } else {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            this.detail.read(cx).text(),
                                        ));
                                    }
                                },
                                cx,
                            ))
                            .child(self.button(
                                "save-cell",
                                "Save Value…",
                                true,
                                |this, _, cx| this.save_cell(cx),
                                cx,
                            ))
                            .child(self.button(
                                "preview-cell",
                                "Open Media…",
                                true,
                                |this, _, cx| this.preview_external(cx),
                                cx,
                            )),
                    );
            }
            Panel::Help => {
                body = body.child(crate::help::render(
                    self.preferences.language,
                    self.preferences.dark,
                ))
            }
            Panel::ColumnDisplay { column, binary } => {
                if let Some(info) = self
                    .result
                    .as_ref()
                    .and_then(|result| result.columns.get(column))
                {
                    let name = info.name.clone();
                    let style = self.formats.get(&name).cloned().unwrap_or_default();
                    body = body.child(format!("{} · {}", name, info.data_type));
                    if binary {
                        body = body.child(
                            controls::segment_group(c).flex_wrap().children(
                                BinaryFormat::all()
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, format)| {
                                        self.binary_capabilities.get(&column).map_or(
                                            matches!(
                                                format,
                                                BinaryFormat::Hex
                                                    | BinaryFormat::Base64
                                                    | BinaryFormat::Size
                                            ),
                                            |available| available.contains(format),
                                        )
                                    })
                                    .map(|(index, format)| {
                                        let format = *format;
                                        let column = name.clone();
                                        controls::segment(
                                            ("binary-format", index),
                                            c,
                                            style.binary == format,
                                        )
                                        .child(t(format.label()))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.formats
                                                    .entry(column.clone())
                                                    .or_default()
                                                    .binary = format;
                                                cx.notify();
                                            }),
                                        )
                                    }),
                            ),
                        );
                    } else {
                        body = body.child(
                            controls::segment_group(c).flex_wrap().children(
                                FloatDisplay::all()
                                    .iter()
                                    .enumerate()
                                    .map(|(index, format)| {
                                        let format = *format;
                                        let column = name.clone();
                                        controls::segment(
                                            ("float-format", index),
                                            c,
                                            style.float == format,
                                        )
                                        .child(t(format.label()))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.formats
                                                    .entry(column.clone())
                                                    .or_default()
                                                    .float = format;
                                                cx.notify();
                                            }),
                                        )
                                    }),
                            ),
                        );
                    }
                }
            }
            Panel::ProfileChartLimit => {
                body = body.child(self.render_profile_chart_limit(cx));
            }
            Panel::About => {
                let version =
                    option_env!("PARQUET_VIEWER_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
                let about_version = t("Version {version} · Rust + GPUI + Arrow + DataFusion")
                    .replace("{version}", version);
                body = body
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .child(icons::app_logo(64.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(px(24.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(t("Parquet Viewer for macOS")),
                                    )
                                    .child(div().text_color(rgb(c.muted)).child(about_version)),
                            ),
                    )
                    .child(t("GPL-3.0-or-later. GPUI components: Apache-2.0."))
            }
        }
        div()
            .id("modal-overlay")
            .occlude()
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .top(px(toolbar_height))
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(c.scrim))
            .p_4()
            .child(
                div()
                    .w(px(match panel {
                        Panel::Range | Panel::CsvJump | Panel::ColumnDisplay { .. } => 420.,
                        Panel::ProfileChartLimit => 460.,
                        Panel::Sort => 680.,
                        _ => 780.,
                    }))
                    .max_w_full()
                    .max_h_full()
                    .when(panel != Panel::CsvImport, |dialog| {
                        dialog.h(px(match panel {
                            Panel::Range if self.awaiting_initial_range() => 340.,
                            Panel::Range => 300.,
                            Panel::CsvJump => 185.,
                            Panel::ProfileChartLimit => 350.,
                            Panel::ColumnDisplay { .. } => 220.,
                            Panel::Export | Panel::AggregateExport | Panel::About => 320.,
                            _ => 560.,
                        }))
                    })
                    .flex()
                    .flex_col()
                    .bg(rgb(c.surface))
                    .border_1()
                    .border_color(rgb(c.border))
                    .rounded(px(10.))
                    .shadow_lg()
                    .overflow_hidden()
                    .when(panel == Panel::CsvImport, |dialog| {
                        dialog.child(self.render_csv_import_heading(cx))
                    })
                    .when(panel != Panel::CsvImport, |dialog| {
                        dialog.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .px_3()
                                .py_2()
                                .flex_shrink_0()
                                .border_b_1()
                                .border_color(rgb(c.border))
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(t(title)),
                                )
                                .child(
                                    controls::icon_button("close-panel", c, true)
                                        .child(icons::glyph(
                                            Icon::Close,
                                            14.,
                                            self.preferences.dark,
                                        ))
                                        .tooltip(icons::tooltip(
                                            t("Close  esc"),
                                            self.preferences.dark,
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.dismiss_panel(window, cx);
                                        })),
                                ),
                        )
                    })
                    .child(body)
                    .when(panel == Panel::CsvImport, |dialog| {
                        dialog.child(self.render_csv_import_actions(cx))
                    })
                    .when_some(self.error.clone(), |el, error| {
                        el.child(
                            div()
                                .p_2()
                                .bg(rgb(c.danger_bg))
                                .text_color(rgb(c.danger))
                                .child(t(error)),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Focusable for Viewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar_layout = toolbar::layout(window);
        if self.focused_panel != self.panel {
            self.focused_panel = self.panel;
            let target = match self.panel {
                Some(Panel::Cell) => self.detail.focus_handle(cx),
                Some(Panel::CsvImport) => self.csv_import_focus(),
                Some(Panel::ProfileChartLimit) => self.profile_chart_limit.focus_handle(cx),
                Some(Panel::Range) => self.offset.focus_handle(cx),
                Some(Panel::CsvJump) => self.csv_jump.focus_handle(cx),
                Some(Panel::SavedViews) => self.saved_view_focus(cx),
                None if self.fields_sidebar_open => self.field_search.focus_handle(cx),
                _ => self.focus.clone(),
            };
            window.defer(cx, move |window, _| target.focus(window));
        }
        let c = Colors::new(self.preferences.dark);
        window.set_window_title(&format!(
            "Parquet Viewer · {} {}",
            t("Window"),
            self.window_color_index.saturating_add(1)
        ));
        div()
            .id("viewer")
            .track_focus(&self.focus)
            .key_context("Viewer")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .font_family("Helvetica Neue")
            .text_size(px(12.))
            .text_color(rgb(c.text))
            .bg(rgb(c.bg))
            .on_action(
                cx.listener(|this, _: &English, _, cx| this.set_language(Language::English, cx)),
            )
            .on_action(cx.listener(|this, _: &SimplifiedChinese, _, cx| {
                this.set_language(Language::SimplifiedChinese, cx)
            }))
            .on_action(
                cx.listener(|this, _: &OpenFile, _, cx| this.open_dialog(OpenDialog::Files, cx)),
            )
            .on_action(cx.listener(|this, _: &ImportTextFile, _, cx| {
                this.open_dialog(OpenDialog::TextFile, cx)
            }))
            .on_action(cx.listener(|this, _: &New, _, cx| {
                this.close_file(cx);
                this.offset.update(cx, |input, cx| input.set_text("0", cx));
                this.count
                    .update(cx, |input, cx| input.set_text("1000", cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseFile, _, cx| this.close_file(cx)))
            .on_action(cx.listener(|this, _: &Reload, _, cx| this.reload(cx)))
            .on_action(cx.listener(|this, _: &Export, _, cx| this.show_panel(Panel::Export, cx)))
            .on_action(cx.listener(|this, _: &OpenSavedViews, _, cx| this.open_saved_views(cx)))
            .on_action(
                cx.listener(|this, _: &Fields, window, cx| this.toggle_fields_sidebar(window, cx)),
            )
            .on_action(cx.listener(|this, _: &OpenStatistics, window, cx| {
                this.toggle_statistics_sidebar(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Execute, _, cx| this.execute(cx)))
            .on_action(cx.listener(|this, _: &LoadRange, _, cx| this.show_panel(Panel::Range, cx)))
            .on_action(cx.listener(|this, _: &LoadAll, _, cx| this.load_all(cx)))
            .on_action(cx.listener(|this, _: &BrowseText, _, cx| this.resume_csv_browse(cx)))
            .on_action(cx.listener(|this, _: &GoToCsvRow, _, cx| this.open_csv_jump(cx)))
            .on_action(cx.listener(|this, _: &Copy, window, cx| {
                if this.input_focused(window, cx) {
                    window.dispatch_action(Box::new(text_input::Copy), cx);
                } else {
                    this.copy_selection(false, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &CopyHeaders, _, cx| this.copy_selection(true, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, window, cx| {
                if this.input_focused(window, cx) {
                    window.dispatch_action(Box::new(text_input::SelectAll), cx);
                } else {
                    this.select_all(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &Inspect, _, cx| {
                if this.panel.is_none() && this.busy.is_none() {
                    this.inspect(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Help, _, cx| this.show_panel(Panel::Help, cx)))
            .on_action(cx.listener(|this, _: &About, _, cx| this.show_panel(Panel::About, cx)))
            .on_action(cx.listener(|this, _: &FocusFilter, window, cx| {
                if !this.filter_sidebar_open {
                    this.toggle_filter_sidebar(window, cx);
                } else if let Some(editor) = &this.filter_editor {
                    editor.read(cx).focus_first(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                if std::mem::take(&mut this.file_menu_open)
                    || this.settings_menu.take().is_some()
                    || this.axis_menu.take().is_some()
                    || this.selection_menu.take().is_some()
                    || this.close_csv_delimiter_menu(cx)
                {
                } else if this.busy.is_some() {
                    this.cancel(cx);
                } else if this.column_profile_running() {
                    this.cancel_column_profile(cx);
                } else if this.panel.is_some() {
                    this.dismiss_panel(window, cx);
                } else if this.fields_sidebar_open {
                    this.dismiss_fields_sidebar(window, cx);
                } else if this.preview.is_some() || this.preview_pending.is_some() {
                    this.close_preview(cx);
                    this.focus.focus(window);
                } else if this.statistics_tab.is_some() {
                    this.close_statistics_sidebar(cx);
                    this.focus.focus(window);
                } else if this.error.take().is_some() {
                } else if this.filter_sidebar_open {
                    this.filter_sidebar_open = false;
                    this.focus.focus(window);
                } else {
                    this.clear_filter(cx);
                }
                cx.notify();
            }))
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if !event.dragging() {
                    this.resize = None;
                    this.sidebar_resize = None;
                    this.scroll_drag = None;
                    this.audio_seek_drag = false;
                    this.selecting = false;
                }
                if let Some((index, start, width)) = this.resize {
                    if let Some(column_width) = this.widths.get_mut(index) {
                        *column_width =
                            (width + f32::from(event.position.x - start)).clamp(70., 1500.);
                        cx.notify();
                    }
                }
                if let Some(resize) = &mut this.sidebar_resize
                    && (resize.dragged || event.position.x != resize.start)
                    && resize.statistics == this.statistics_tab.is_some()
                {
                    resize.dragged = true;
                    let viewport = f32::from(window.viewport_size().width);
                    let width = sidebar_width(
                        viewport,
                        resize.width - f32::from(event.position.x - resize.start),
                    );
                    if resize.statistics {
                        this.statistics_width = Some(width);
                    } else {
                        this.sidebar_width = Some(width);
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.resize = None;
                    if this.sidebar_resize.take().is_some() {
                        cx.notify();
                    }
                    this.selecting = false;
                    this.scroll_drag = None;
                    this.audio_seek_drag = false;
                }),
            )
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if !this.focus.is_focused(window)
                    || this.panel.is_some()
                    || this.selection_menu.is_some()
                    || this.axis_menu.is_some()
                    || this.settings_menu.is_some()
                    || this.file_menu_open
                    || this.result.is_none()
                {
                    return;
                }
                let Some((anchor, (row, col))) = this.selection else {
                    return;
                };
                let result = this.result.as_ref().unwrap();
                let last_row = this.table_row_count().saturating_sub(1);
                let last_column = result.columns.len().saturating_sub(1);
                let jump = event.keystroke.modifiers.platform || event.keystroke.modifiers.control;
                let next = match event.keystroke.key.as_str() {
                    "up" => (if jump { 0 } else { row.saturating_sub(1) }, col),
                    "down" => (
                        if jump {
                            last_row
                        } else {
                            (row + 1).min(last_row)
                        },
                        col,
                    ),
                    "left" => (row, if jump { 0 } else { col.saturating_sub(1) }),
                    "right" => (
                        row,
                        if jump {
                            last_column
                        } else {
                            (col + 1).min(last_column)
                        },
                    ),
                    "home" => (if jump { 0 } else { row }, 0),
                    "end" => (if jump { last_row } else { row }, last_column),
                    _ => return,
                };
                this.selection = Some((
                    if event.keystroke.modifiers.shift {
                        anchor
                    } else {
                        next
                    },
                    next,
                ));
                this.selection_kind = SelectionKind::Cells;
                this.reveal_row(next.0);
                this.reveal_column(next.1, window);
                cx.notify();
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.open(paths.paths().to_vec(), cx)
            }))
            .child(div().h(px(toolbar_layout.height)).flex_shrink_0())
            .when_some(
                self.info
                    .as_ref()
                    .and_then(|info| info.warnings.first())
                    .cloned(),
                |el, warning| {
                    el.child(
                        div()
                            .px_3()
                            .py_2()
                            .bg(rgb(c.warning_bg))
                            .text_color(rgb(c.warning))
                            .child(t(warning)),
                    )
                },
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .bg(rgb(c.danger_bg))
                        .text_color(rgb(c.danger))
                        .child(div().flex_1().min_w_0().whitespace_normal().child(t(error)))
                        .child(self.button(
                            "dismiss-error",
                            "Dismiss",
                            true,
                            |this, _, cx| {
                                this.error = None;
                                cx.notify();
                            },
                            cx,
                        )),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.render_workspace(window, cx)),
                    )
                    .when_some(
                        Some(self.right_sidebar_width(window)).filter(|width| *width > 0.),
                        |el, width| el.child(self.render_right_sidebar(width, cx)),
                    ),
            )
            .child(self.render_status_bar(window, cx))
            .when(self.panel.is_some(), |el| {
                el.child(self.render_panel(toolbar_layout.height, cx))
            })
            .when(self.selection_menu.is_some(), |el| {
                el.child(self.render_selection_menu(window, cx))
            })
            .when(self.axis_menu.is_some(), |el| {
                el.child(self.render_axis_menu(window, cx))
            })
            .when(self.settings_menu.is_some(), |el| {
                el.child(self.render_settings_menu(window, toolbar_layout.height, cx))
            })
            .when(self.file_menu_open, |el| {
                el.child(self.render_file_menu(window, toolbar_layout.height, cx))
            })
            .child(
                self.render_commands(window, toolbar_layout, cx)
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0(),
            )
    }
}

fn needs_parquet_startup_prompt(csv_options: Option<&CsvOptions>, file_size: u64) -> bool {
    const PROMPT_AT_BYTES: u64 = 5_000_000_000;
    csv_options.is_none() && file_size >= PROMPT_AT_BYTES
}

fn read_source(
    paths: &[PathBuf],
    prepared: Option<CsvPrepared>,
    cancel: &AtomicBool,
) -> anyhow::Result<(Engine, SourceStamps)> {
    let engine = if let (Some(prepared), [path]) = (prepared, paths) {
        Engine::open_prepared_csv(path, prepared, cancel)?
    } else {
        Engine::open_cancellable(paths, cancel)?
    };
    let stamps = engine.file_stamps(250);
    Ok((engine, stamps))
}

fn stamp(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

pub(crate) fn pretty_json(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_owned())
}
fn tsv(text: &str) -> String {
    if text.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}
pub(crate) fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(ImageFormat::Gif)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(ImageFormat::Webp)
    } else if bytes.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some(ImageFormat::Tiff)
    } else if std::str::from_utf8(bytes).ok().is_some_and(|text| {
        text.trim_start().starts_with("<svg")
            || text.trim_start().starts_with("<?xml") && text.contains("<svg")
    }) {
        Some(ImageFormat::Svg)
    } else {
        None
    }
}
pub(crate) fn binary_extension(bytes: &[u8]) -> &'static str {
    match image_format(bytes) {
        Some(ImageFormat::Png) => "png",
        Some(ImageFormat::Jpeg) => "jpg",
        Some(ImageFormat::Gif) => "gif",
        Some(ImageFormat::Webp) => "webp",
        Some(ImageFormat::Bmp) => "bmp",
        Some(ImageFormat::Tiff) => "tiff",
        Some(ImageFormat::Svg) => "svg",
        _ => {
            if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
                "wav"
            } else if bytes.starts_with(b"ID3")
                || bytes.first() == Some(&0xff)
                    && bytes.get(1).is_some_and(|value| value & 0xe0 == 0xe0)
            {
                "mp3"
            } else {
                "bin"
            }
        }
    }
}
pub(crate) fn is_audio(bytes: &[u8]) -> bool {
    matches!(binary_extension(bytes), "wav" | "mp3")
}

fn update_shared_preferences(update: PreferenceUpdate, cx: &mut App) -> Preferences {
    let update_menus = matches!(&update, PreferenceUpdate::Language(_));
    let (preferences, changed) = {
        let shared = cx.global_mut::<SharedPreferences>();
        let mut next = shared.0.clone();
        next.apply_update(update);
        let changed = next != shared.0;
        if changed {
            shared.0 = next.clone();
        }
        (next, changed)
    };
    if update_menus {
        i18n::set_language(preferences.language);
        install_menus(cx);
    }
    if changed {
        preferences.save();
    }
    preferences
}

fn select_application_language(language: Language, cx: &mut App) {
    cx.defer(move |cx| {
        update_shared_preferences(PreferenceUpdate::Language(language), cx);
    });
}

fn main_windows(cx: &App) -> Vec<gpui::WindowHandle<Viewer>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<Viewer>())
        .collect()
}

fn preferred_main_window(cx: &App) -> Option<gpui::WindowHandle<Viewer>> {
    let windows = main_windows(cx);
    if let Some(active) = cx.active_window() {
        if let Some(main) = active.downcast::<Viewer>() {
            return Some(main);
        }
    }
    cx.global::<MainWindowRegistry>()
        .last_active
        .filter(|last| {
            windows
                .iter()
                .any(|window| window.window_id() == last.window_id())
        })
        .or_else(|| windows.first().copied())
}

/// Finder opens may reuse a blank window, but must not replace an open dataset
/// or an import/field-selection draft merely because that window was active.
fn reusable_main_window<T: std::marker::Copy + PartialEq>(
    candidates: &[(T, bool)],
    preferred: Option<T>,
) -> Option<T> {
    preferred
        .filter(|preferred| {
            candidates
                .iter()
                .any(|(id, empty)| id == preferred && *empty)
        })
        .or_else(|| {
            candidates
                .iter()
                .find_map(|(id, empty)| empty.then_some(*id))
        })
}

fn route_external_paths(paths: Vec<PathBuf>, cx: &mut App) {
    if paths.is_empty() {
        return;
    }
    let windows = main_windows(cx);
    let candidates: Vec<_> = windows
        .iter()
        .map(|window| {
            let reusable = window.read(cx).is_ok_and(|view| {
                view.engine.is_none()
                    && view.paths.is_empty()
                    && view.busy.is_none()
                    && view.panel.is_none()
            });
            (window.window_id(), reusable)
        })
        .collect();
    let target = reusable_main_window(
        &candidates,
        preferred_main_window(cx).map(|window| window.window_id()),
    );
    if let Some(window) = windows
        .into_iter()
        .find(|window| Some(window.window_id()) == target)
    {
        let _ = window.update(cx, |this, window, cx| {
            window.activate_window();
            this.open(paths, cx);
        });
    } else if let Err(error) = open_main_window(paths, cx) {
        log::error!("Could not open a dataset window: {error:#}");
    }
}

fn open_main_window(
    paths: Vec<PathBuf>,
    cx: &mut App,
) -> anyhow::Result<gpui::WindowHandle<Viewer>> {
    let color_index = {
        let registry = cx.global_mut::<MainWindowRegistry>();
        let index = registry.next_color_index;
        registry.next_color_index = index.saturating_add(1);
        index
    };
    let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(1000.), px(500.))),
            titlebar: Some(window_chrome::options(Some("Parquet Viewer".into()))),
            ..Default::default()
        },
        |_, cx| cx.new(|cx| Viewer::new(color_index, cx)),
    )?;
    handle.update(cx, |this, window, cx| {
        this.main_window = Some(handle);
        this.observe_system_theme(window, cx);
        this.window_active = window.is_window_active();
        this.resources.set_active(this.window_active);
        let subscription = cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            this.resources.set_active(this.window_active);
            this.display_cache
                .borrow_mut()
                .trim(this.resources.cache_limit_bytes());
            if window.is_window_active() {
                cx.global_mut::<MainWindowRegistry>().last_active = this.main_window;
            }
        });
        this._subscriptions.push(subscription);
        this.focus.focus(window);
        if !paths.is_empty() {
            this.open(paths, cx);
        }
    })?;
    cx.global_mut::<MainWindowRegistry>().last_active = Some(handle);
    Ok(handle)
}

fn request_new_window(cx: &mut App) {
    cx.defer(|cx| {
        if let Err(error) = open_main_window(Vec::new(), cx) {
            log::error!("Could not create a dataset window: {error:#}");
        }
    });
}

fn start_process_memory_display(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            let sampled = cx
                .background_executor()
                .spawn(async { crate::process_memory::sample() })
                .await;
            if cx
                .update(|cx| {
                    if cx.global::<MainWindowRegistry>().process_memory_bytes == sampled {
                        return;
                    }
                    cx.global_mut::<MainWindowRegistry>().process_memory_bytes = sampled;
                    for window in main_windows(cx) {
                        let _ = window.update(cx, |_, _, cx| {
                            cx.notify();
                        });
                    }
                })
                .is_err()
            {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
        }
    })
    .detach();
}

fn activate_main_window(cx: &mut App) {
    cx.defer(|cx| {
        if let Some(window) = preferred_main_window(cx) {
            let _ = window.update(cx, |_, window, _| window.activate_window());
        }
    });
}

fn cycle_window(previous: bool, cx: &mut App) {
    cx.defer(move |cx| {
        let mut windows: Vec<_> = cx
            .windows()
            .into_iter()
            .filter(|window| window.downcast::<Viewer>().is_some())
            .collect();
        windows.sort_by_key(|window| window.window_id().as_u64());
        if windows.is_empty() {
            return;
        }
        let active = cx.active_window().or_else(|| {
            cx.window_stack()
                .and_then(|windows| windows.first().copied())
        });
        let index = active
            .and_then(|active| windows.iter().position(|window| *window == active))
            .map(|index| {
                if previous {
                    (index + windows.len() - 1) % windows.len()
                } else {
                    (index + 1) % windows.len()
                }
            })
            .unwrap_or(if previous { windows.len() - 1 } else { 0 });
        let _ = windows[index].update(cx, |_, window, _| window.activate_window());
    });
}

pub fn run() {
    let initial: Vec<PathBuf> = std::env::args_os()
        .skip(1)
        .filter(|arg| !arg.to_string_lossy().starts_with("-psn_"))
        .map(PathBuf::from)
        .collect();
    let mut preferences = Preferences::load();
    i18n::set_language(preferences.language);
    let app = Application::new();
    let open_queue: Arc<Mutex<Vec<Vec<PathBuf>>>> = Arc::new(Mutex::new(Vec::new()));
    let queue = open_queue.clone();
    app.on_open_urls(move |urls| {
        let paths: Vec<_> = urls
            .into_iter()
            .filter_map(|url| url::Url::parse(&url).ok()?.to_file_path().ok())
            .collect();
        if !paths.is_empty() {
            queue.lock().unwrap().push(paths);
        }
    });
    app.run(move |cx: &mut App| {
        #[cfg(target_os = "macos")]
        if let Err(error) = crate::macos_icon::install() {
            log::warn!("Could not set the application icon: {error}");
        }
        preferences.dark = preferences
            .theme_mode
            .resolve(theme::system_appearance(cx.window_appearance()));
        cx.set_global(SharedPreferences(preferences));
        cx.set_global(MainWindowRegistry::default());
        text_input::init(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-o", OpenFile, None),
            KeyBinding::new("cmd-n", NewWindow, None),
            KeyBinding::new("cmd-shift-n", CloseFile, Some("Viewer")),
            KeyBinding::new("cmd-r", Reload, None),
            KeyBinding::new("cmd-s", Export, None),
            KeyBinding::new("cmd-shift-s", Export, None),
            KeyBinding::new("cmd-shift-v", OpenSavedViews, Some("Viewer")),
            KeyBinding::new("cmd-e", LoadAll, None),
            KeyBinding::new("cmd-f", Fields, None),
            KeyBinding::new("ctrl-f", Fields, None),
            KeyBinding::new("cmd-alt-f", FocusFilter, None),
            KeyBinding::new("cmd-alt-g", OpenStatistics, None),
            KeyBinding::new("cmd-c", Copy, Some("Viewer")),
            KeyBinding::new("cmd-shift-c", CopyHeaders, Some("Viewer")),
            KeyBinding::new("cmd-a", SelectAll, Some("Viewer")),
            KeyBinding::new("enter", Inspect, Some("Viewer")),
            KeyBinding::new("enter", text_input::Submit, Some("CsvDelimiter")),
            KeyBinding::new("escape", Dismiss, None),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-`", NextWindow, None),
            KeyBinding::new("cmd-shift-`", PreviousWindow, None),
            // macOS represents Shift+` as the printable ~ character.
            KeyBinding::new("cmd-~", PreviousWindow, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &NewWindow, cx| request_new_window(cx));
        cx.on_action(|_: &English, cx| select_application_language(Language::English, cx));
        cx.on_action(|_: &SimplifiedChinese, cx| {
            select_application_language(Language::SimplifiedChinese, cx)
        });
        cx.on_action(|_: &MainWindow, cx| activate_main_window(cx));
        cx.on_action(|_: &NextWindow, cx| cycle_window(false, cx));
        cx.on_action(|_: &PreviousWindow, cx| cycle_window(true, cx));
        install_menus(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        if let Err(error) = open_main_window(initial, cx) {
            log::error!("Could not open the initial dataset window: {error:#}");
            cx.quit();
            return;
        }
        start_process_memory_display(cx);
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let batches: Vec<_> = open_queue.lock().unwrap().drain(..).collect();
                if !batches.is_empty()
                    && cx
                        .update(|cx| {
                            for paths in batches {
                                route_external_paths(paths, cx);
                            }
                        })
                        .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.activate(true);
    });
}

fn install_menus(cx: &mut App) {
    cx.set_menus(vec![
        Menu {
            name: t("Parquet Viewer").into(),
            items: vec![
                MenuItem::action(t("About Parquet Viewer"), About),
                MenuItem::separator(),
                MenuItem::action(t("Quit Parquet Viewer"), Quit),
            ],
        },
        Menu {
            name: t("Window").into(),
            items: vec![
                MenuItem::action(t("Main Window"), MainWindow),
                MenuItem::separator(),
                MenuItem::action(t("Next Window"), NextWindow),
                MenuItem::action(t("Previous Window"), PreviousWindow),
            ],
        },
        Menu {
            name: t("Help").into(),
            items: vec![MenuItem::action(t("Table & Filter Help"), Help)],
        },
        Menu {
            name: t("Language").into(),
            items: vec![
                MenuItem::action("English", English),
                MenuItem::action("简体中文", SimplifiedChinese),
            ],
        },
    ]);
}

#[cfg(test)]
mod multi_window_tests {
    use super::{
        CsvOptions, PreferenceUpdate, Preferences, ThemeMode, needs_parquet_startup_prompt,
        reusable_main_window,
    };
    use std::path::PathBuf;

    #[test]
    fn delimited_files_skip_the_startup_range_prompt_at_any_size() {
        let text = CsvOptions::default();
        for size in [512, 4_999_999_999, 5_000_000_000, 5_000_000_001, u64::MAX] {
            assert!(!needs_parquet_startup_prompt(Some(&text), size));
        }
    }

    #[test]
    fn parquet_startup_prompt_starts_at_five_decimal_gigabytes() {
        assert!(!needs_parquet_startup_prompt(None, 512));
        assert!(!needs_parquet_startup_prompt(None, 4_999_999_999));
        assert!(needs_parquet_startup_prompt(None, 5_000_000_000));
        assert!(needs_parquet_startup_prompt(None, u64::MAX));
    }

    #[test]
    fn interleaved_window_changes_do_not_overwrite_shared_theme_or_recent_files() {
        let mut shared = Preferences::default();
        let stale_second_window = shared.clone();
        shared.apply_update(PreferenceUpdate::Theme(ThemeMode::Dark, true));
        shared.apply_update(PreferenceUpdate::Recent(vec![PathBuf::from(
            "/tmp/first.parquet",
        )]));
        shared.apply_update(PreferenceUpdate::Recent(vec![PathBuf::from(
            "/tmp/second.parquet",
        )]));
        // This window was opened before the theme and recent-file changes.
        // Its unrelated update must not serialize that old preference cache.
        shared.apply_update(PreferenceUpdate::ProfileChartLimits {
            rows: None,
            category_bars: Some(17),
        });
        shared.apply_update(PreferenceUpdate::ProfileChartLimits {
            rows: Some(stale_second_window.profile_chart_max_rows / 2),
            category_bars: None,
        });
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&shared).unwrap()).unwrap();
        assert_eq!(restored.theme_mode, ThemeMode::Dark);
        assert!(restored.dark);
        assert_eq!(restored.profile_chart_max_rows, 1_000_000);
        assert_eq!(restored.profile_chart_category_bars, 17);
        assert_eq!(
            restored.recent,
            [
                PathBuf::from("/tmp/second.parquet"),
                PathBuf::from("/tmp/first.parquet")
            ]
        );
    }

    #[test]
    fn legacy_date_display_preserves_other_saved_preferences() {
        for date in [
            serde_json::json!("Local"),
            serde_json::json!("Iso"),
            serde_json::json!({ "Custom": "yyyy-MM-dd" }),
        ] {
            let preferences: Preferences = serde_json::from_value(serde_json::json!({
                "date": date,
                "dark": true,
                "always_load_all": true,
                "profile_chart_max_rows": 300_000,
            }))
            .unwrap();
            assert_eq!(preferences.theme_mode, ThemeMode::Dark);
            assert_eq!(preferences.profile_chart_max_rows, 300_000);
            let saved = serde_json::to_value(preferences).unwrap();
            assert!(saved.get("date").is_none());
            assert!(saved.get("always_load_all").is_none());
        }
    }

    #[test]
    fn profile_chart_limit_defaults_for_old_preferences_and_persists_overrides() {
        let existing: Preferences = serde_json::from_str(r#"{"dark":true}"#).unwrap();
        assert_eq!(existing.profile_chart_max_rows, 2_000_000);
        assert_eq!(existing.profile_chart_category_bars, 8);
        assert_eq!(Preferences::default().profile_chart_max_rows, 2_000_000);
        assert_eq!(Preferences::default().profile_chart_category_bars, 8);
        for (rows, category_bars) in [
            (0, 0),
            (0, 1),
            (300_000, 15),
            (2_000_000, 100),
            (1, 101),
            (usize::MAX, usize::MAX),
        ] {
            let mut shared = existing.clone();
            shared.apply_update(PreferenceUpdate::ProfileChartLimits {
                rows: Some(rows),
                category_bars: Some(category_bars),
            });
            let saved = serde_json::to_vec(&shared).unwrap();
            let restored: Preferences = serde_json::from_slice(&saved).unwrap();
            assert_eq!(restored.profile_chart_max_rows, rows);
            assert_eq!(restored.profile_chart_category_bars, category_bars);
            assert!(restored.dark);
        }
    }

    #[test]
    fn finder_reuses_only_live_blank_windows_and_never_replaces_loaded_datasets() {
        let windows = [(1, false), (2, true), (3, true)];
        assert_eq!(reusable_main_window(&windows, Some(3)), Some(3));
        assert_eq!(reusable_main_window(&windows, Some(1)), Some(2));
        assert_eq!(reusable_main_window(&windows, Some(99)), Some(2));
        assert_eq!(
            reusable_main_window(&[(1, false), (2, false)], Some(1)),
            None
        );
        assert_eq!(reusable_main_window(&[(2, false)], Some(1)), None);
        assert_eq!(reusable_main_window::<u64>(&[], None), None);
    }
}

#[cfg(test)]
mod theme_preferences_tests {
    use super::{Preferences, ThemeMode};
    use crate::i18n::Language;
    use gpui::WindowAppearance;
    use std::path::PathBuf;

    #[test]
    fn legacy_theme_choices_and_other_preferences_survive_migration() {
        for (dark, mode) in [(false, ThemeMode::Light), (true, ThemeMode::Dark)] {
            let saved = serde_json::json!({
                "dark": dark,
                "always_load_all": true,
                "always_select_all_fields": true,
                "csv_inference_rows": 42,
                "recent": ["/tmp/中文.parquet"],
                "language": "Turkish"
            });
            let preferences: Preferences = serde_json::from_value(saved).unwrap();
            assert_eq!(preferences.theme_mode, mode);
            assert_eq!(preferences.dark, dark);
            assert_eq!(mode.resolve(WindowAppearance::Dark), dark);
            assert_eq!(mode.resolve(WindowAppearance::Light), dark);
            assert_eq!(preferences.language, Language::English);
            assert_eq!(preferences.recent, [PathBuf::from("/tmp/中文.parquet")]);
            assert!(
                serde_json::to_value(preferences)
                    .unwrap()
                    .get("csv_inference_rows")
                    .is_none()
            );
        }
    }

    #[test]
    fn fresh_preferences_follow_system_without_inventing_a_legacy_choice() {
        let empty: Preferences = serde_json::from_str("{}").unwrap();
        let unrelated: Preferences = serde_json::from_str(r#"{"always_load_all":true}"#).unwrap();
        for preferences in [Preferences::default(), empty, unrelated] {
            assert_eq!(preferences.theme_mode, ThemeMode::System);
            assert!(!preferences.theme_mode.resolve(WindowAppearance::Light));
            assert!(preferences.theme_mode.resolve(WindowAppearance::Dark));
        }
    }

    #[test]
    fn explicit_mode_wins_over_the_legacy_last_effective_dark_value() {
        for (mode, stored_dark, expected_dark) in [
            ("light", true, false),
            ("dark", false, true),
            ("system", true, true),
            ("system", false, false),
        ] {
            let preferences: Preferences = serde_json::from_value(serde_json::json!({
                "theme_mode": mode,
                "dark": stored_dark
            }))
            .unwrap();
            assert_eq!(preferences.dark, expected_dark);
            if mode == "system" {
                // Last session's effective color must never lock System mode.
                assert_eq!(preferences.theme_mode, ThemeMode::System);
                assert!(!preferences.theme_mode.resolve(WindowAppearance::Light));
                assert!(preferences.theme_mode.resolve(WindowAppearance::Dark));
            }
        }
    }

    #[test]
    fn saving_and_reloading_preserves_all_three_theme_modes() {
        for mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            let preferences = Preferences {
                theme_mode: mode,
                dark: mode.resolve(WindowAppearance::Dark),
                recent: vec![PathBuf::from("/tmp/example.parquet")],
                profile_chart_max_rows: 300_000,
                ..Preferences::default()
            };
            let json = serde_json::to_vec(&preferences).unwrap();
            let restored: Preferences = serde_json::from_slice(&json).unwrap();
            assert_eq!(restored.theme_mode, mode);
            assert_eq!(restored.dark, preferences.dark);
            assert_eq!(restored.recent, preferences.recent);
            assert_eq!(restored.profile_chart_max_rows, 300_000);
        }
    }
}

#[cfg(test)]
mod variable_row_tests {
    use super::{
        ColumnInfo, ColumnStyle, RowLayoutKey, RowsScrollState, TABLE_ROW_MIN_HEIGHT,
        column_width_sample_rows,
    };
    use crate::formatting::FloatDisplay;
    use gpui::{ListAlignment, ListOffset, ListState, px};
    use std::collections::HashMap;

    #[test]
    fn ordinary_table_millions_of_rows_use_constant_height_storage() {
        let rows = RowsScrollState::new();
        rows.reset(10_000_000);
        for pane in [&rows.main, &rows.frozen] {
            assert_eq!(pane.fixed_item_height(), Some(px(TABLE_ROW_MIN_HEIGHT)));
            assert_eq!(pane.item_count(), 10_000_000);
            assert_eq!(pane.max_offset_for_scrollbar().height, px(280_000_000.));
        }
        rows.main.scroll_to(ListOffset {
            item_ix: 9_999_999,
            offset_in_item: px(3.),
        });
        rows.sync_from(false);
        assert_eq!(rows.frozen.logical_scroll_top().item_ix, 9_999_999);
        assert_eq!(rows.frozen.logical_scroll_top().offset_in_item, px(3.));
        rows.reset(0);
        assert_eq!(rows.main.item_count(), 0);
        assert_eq!(rows.frozen.item_count(), 0);
    }

    #[test]
    fn switching_wrap_storage_preserves_both_panes_logical_scroll_position() {
        let rows = RowsScrollState::new();
        rows.reset(100);
        rows.main.scroll_to(ListOffset {
            item_ix: 51,
            offset_in_item: px(9.),
        });
        rows.sync_from(false);
        assert!(rows.set_fixed_height(false));
        for pane in [&rows.main, &rows.frozen] {
            assert_eq!(pane.fixed_item_height(), None);
            assert_eq!(pane.item_count(), 100);
            assert_eq!(pane.logical_scroll_top().item_ix, 51);
            assert_eq!(pane.logical_scroll_top().offset_in_item, px(9.));
        }
        assert!(!rows.set_fixed_height(false));
        assert!(rows.set_fixed_height(true));
        for pane in [&rows.main, &rows.frozen] {
            assert_eq!(pane.fixed_item_height(), Some(px(TABLE_ROW_MIN_HEIGHT)));
            assert_eq!(pane.logical_scroll_top().item_ix, 51);
            assert_eq!(pane.logical_scroll_top().offset_in_item, px(9.));
        }
    }

    #[test]
    fn automatic_column_width_samples_the_loaded_window_without_scanning_it() {
        assert!(column_width_sample_rows(0).is_empty());
        assert_eq!(column_width_sample_rows(3), [0, 1, 2]);
        for count in [256, 257, 10_000_000, usize::MAX] {
            let rows = column_width_sample_rows(count);
            assert_eq!(rows.len(), 256);
            assert_eq!(rows.first(), Some(&0));
            assert_eq!(rows.last(), Some(&(count - 1)));
            assert!(rows.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(rows.iter().all(|&row| row < count));
        }
    }

    #[test]
    fn only_wrapped_column_changes_invalidate_row_geometry() {
        let columns = ["value", "notes"].map(|name| ColumnInfo {
            name: name.into(),
            data_type: "VARCHAR".into(),
            nullable: true,
        });
        let mut previous = RowLayoutKey {
            generation: 1,
            snapshot: "snapshot".into(),
            widths: vec![100., 200.],
            formats: HashMap::new(),
        };
        let mut next = previous.clone();
        next.widths[0] = 300.;
        assert!(!next.changes_row_heights(&previous, &columns));

        next.formats.insert(
            "notes".into(),
            ColumnStyle {
                word_wrap: true,
                ..ColumnStyle::default()
            },
        );
        assert!(next.changes_row_heights(&previous, &columns));
        previous = next.clone();
        // Starting/cancelling background work changes its token, not the
        // currently displayed snapshot or any of its row heights.
        next.generation += 1;
        assert!(!next.changes_row_heights(&previous, &columns));
        next.widths[0] = 400.;
        assert!(!next.changes_row_heights(&previous, &columns));
        next.widths[1] = 300.;
        assert!(next.changes_row_heights(&previous, &columns));
        previous = next.clone();
        next.formats.get_mut("notes").unwrap().float = FloatDisplay::all()[1];
        assert!(next.changes_row_heights(&previous, &columns));
        next.formats.remove("notes");
        assert!(next.changes_row_heights(&previous, &columns));
    }

    #[test]
    fn estimates_allow_far_navigation_without_measuring_every_row() {
        let rows = ListState::new_with_estimated_item_height(
            10_000,
            ListAlignment::Top,
            px(120.),
            px(TABLE_ROW_MIN_HEIGHT),
        );
        assert_eq!(rows.max_offset_for_scrollbar().height, px(280_000.));
        rows.scroll_by(px(140_000.));
        assert_eq!(rows.logical_scroll_top().item_ix, 5_000);
        assert_eq!(rows.scroll_px_offset_for_scrollbar().y, -px(140_000.));
        rows.reset(2);
        assert_eq!(rows.max_offset_for_scrollbar().height, px(56.));
        assert_eq!(rows.logical_scroll_top().item_ix, 0);
    }

    #[test]
    fn original_constructor_and_zero_invalid_estimates_keep_zero_height() {
        assert_eq!(
            ListState::new(20, ListAlignment::Top, px(0.))
                .max_offset_for_scrollbar()
                .height,
            px(0.)
        );
        for estimate in [0., -28., f32::NAN, f32::INFINITY] {
            let rows = ListState::new_with_estimated_item_height(
                20,
                ListAlignment::Top,
                px(0.),
                px(estimate),
            );
            assert_eq!(rows.max_offset_for_scrollbar().height, px(0.));
            rows.reset(0);
            assert_eq!(rows.item_count(), 0);
        }
    }

    #[test]
    fn panes_share_logical_position_and_reset_together() {
        let rows = RowsScrollState::new();
        rows.reset(10_000);
        rows.frozen.scroll_to(ListOffset {
            item_ix: 4_500,
            offset_in_item: px(9.),
        });
        rows.sync_from(true);
        assert_eq!(rows.main.logical_scroll_top().item_ix, 4_500);
        assert_eq!(rows.main.logical_scroll_top().offset_in_item, px(9.));
        rows.invalidate(10_000);
        assert_eq!(rows.main.logical_scroll_top().item_ix, 4_500);
        assert_eq!(rows.frozen.logical_scroll_top().item_ix, 4_500);
        assert_eq!(rows.main.logical_scroll_top().offset_in_item, px(0.));
        rows.reset(12);
        assert_eq!(rows.main.item_count(), 12);
        assert_eq!(rows.frozen.item_count(), 12);
        assert_eq!(rows.main.logical_scroll_top().item_ix, 0);
        assert_eq!(rows.frozen.logical_scroll_top().item_ix, 0);
    }

    #[test]
    fn scrollbar_bottom_anchors_after_last_item_not_estimated_pixel_position() {
        let rows = RowsScrollState::new();
        rows.reset(4_800);
        rows.main.scroll_to(ListOffset {
            item_ix: 4_775,
            offset_in_item: px(15.),
        });
        rows.sync_from(false);
        rows.scrollbar_drag_started();
        let bottom = rows.main.max_offset_for_scrollbar().height;
        rows.set_offset(bottom);
        for pane in [&rows.main, &rows.frozen] {
            assert_eq!(pane.logical_scroll_top().item_ix, pane.item_count());
            assert_eq!(pane.logical_scroll_top().offset_in_item, px(0.));
        }

        // The scrollbar range remains frozen while content height/count can
        // change. The endpoint must follow the current last item, not the
        // item that happened to occupy the original estimated pixel offset.
        rows.main.splice(0..0, 10);
        rows.frozen.splice(0..0, 10);
        assert_eq!(rows.main.max_offset_for_scrollbar().height, bottom);
        rows.set_offset(bottom);
        assert_eq!(rows.main.logical_scroll_top().item_ix, 4_810);
        assert_eq!(rows.frozen.logical_scroll_top().item_ix, 4_810);
        rows.scrollbar_drag_ended();

        rows.set_offset(px(0.));
        assert_eq!(rows.main.logical_scroll_top().item_ix, 0);
        assert_eq!(rows.frozen.logical_scroll_top().item_ix, 0);
    }

    #[test]
    fn scrollbar_endpoint_anchors_handle_empty_results() {
        let rows = RowsScrollState::new();
        rows.reset(0);
        rows.set_offset(px(0.));
        rows.set_offset(px(100.));
        assert_eq!(rows.main.logical_scroll_top().item_ix, 0);
        assert_eq!(rows.frozen.logical_scroll_top().item_ix, 0);
        assert_eq!(rows.main.logical_scroll_top().offset_in_item, px(0.));
    }
}
