//! Lucide 1.65.0 command artwork (ISC; Feather-derived icons retain MIT),
//! embedded and cached without an AssetSource, file reads or runtime network.
//! See packaging/assets/README.md and THIRD_PARTY_NOTICES.md for the pinned source.
//! Use visible command labels and
//! tooltips alongside the glyphs; this GPUI version has no ARIA/AX label API.

use std::sync::{Arc, OnceLock};

use gpui::{
    AnyView, App, Context, Image, ImageFormat, Img, IntoElement, ObjectFit, Render, SharedString,
    Window, div, img, prelude::*, px, rgb,
};

use crate::theme::Colors;

/// The same original artwork used by the macOS application bundle. Embed it so
/// branding also works when the executable is run directly during development.
pub fn app_logo(size: f32) -> Img {
    static IMAGE: OnceLock<Arc<Image>> = OnceLock::new();
    img(IMAGE
        .get_or_init(|| {
            Arc::new(Image::from_bytes(
                ImageFormat::Png,
                include_bytes!("../packaging/assets/app-icon-1024.png").to_vec(),
            ))
        })
        .clone())
    .size(px(size))
    .object_fit(ObjectFit::Contain)
    .flex_shrink_0()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum Icon {
    Open,
    Folder,
    Import,
    Filter,
    Execute,
    Clear,
    Fields,
    Export,
    Reload,
    Settings,
    Info,
    Freeze,
    Unfreeze,
    Wrap,
    Copy,
    Play,
    Pause,
    Stop,
    Next,
    Back,
    Close,
    Search,
    Table,
    Code,
    Image,
    Audio,
    Check,
    ExternalLink,
    Sun,
    Moon,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Expand,
    Columns,
    RowRange,
    AllRows,
    Calendar,
    ReadOnly,
    Minimize,
    Statistics,
}

impl Icon {
    pub const ALL: &'static [Self] = &[
        Self::Open,
        Self::Folder,
        Self::Import,
        Self::Filter,
        Self::Execute,
        Self::Clear,
        Self::Fields,
        Self::Export,
        Self::Reload,
        Self::Settings,
        Self::Info,
        Self::Freeze,
        Self::Unfreeze,
        Self::Wrap,
        Self::Copy,
        Self::Play,
        Self::Pause,
        Self::Stop,
        Self::Next,
        Self::Back,
        Self::Close,
        Self::Search,
        Self::Table,
        Self::Code,
        Self::Image,
        Self::Audio,
        Self::Check,
        Self::ExternalLink,
        Self::Sun,
        Self::Moon,
        Self::MoveLeft,
        Self::MoveRight,
        Self::MoveUp,
        Self::MoveDown,
        Self::Expand,
        Self::Columns,
        Self::RowRange,
        Self::AllRows,
        Self::Calendar,
        Self::ReadOnly,
        Self::Minimize,
        Self::Statistics,
    ];

    /// Application command identifiers, not translated labels or user data.
    pub fn for_command(command: &str) -> Option<Self> {
        let command = command
            .strip_prefix("peek-")
            .or_else(|| command.strip_prefix("context-"))
            .unwrap_or(command);
        Some(match command {
            "open" | "empty-open" | "open-file" => Self::Open,
            "folder" | "open-folder" => Self::Folder,
            "import" => Self::Import,
            "filter" | "filter-help" | "focus-filter" | "visual-filters" => Self::Filter,
            "execute" | "run" | "apply-query" => Self::Execute,
            "clear" | "clear-filter" | "clear-fields" => Self::Clear,
            "fields" | "choose-fields" => Self::Fields,
            "apply-fields" | "apply" | "confirm" => Self::Check,
            "import-apply-field" => Self::Check,
            "import-preview" => Self::Reload,
            "import-header" | "import-empty" | "import-trim" | "import-case" => return None,
            "reload" => Self::Reload,
            "settings" | "preferences" | "column-options" | "columnoptions" | "import-settings" => {
                Self::Settings
            }
            "readonly" | "read-only" | "readonly-source" | "source-readonly" => Self::ReadOnly,
            "help" | "about" | "info" => Self::Info,
            "freeze" => Self::Freeze,
            "unfreeze" => Self::Unfreeze,
            "wrap" => Self::Wrap,
            "play" | "audio-play" => Self::Play,
            "pause" | "audio-pause" => Self::Pause,
            "stop" | "audio-stop" => Self::Stop,
            "next" | "seek-forward" | "audio-forward" => Self::Next,
            "back" | "return" | "nested-back" | "seek-back" | "audio-back" => Self::Back,
            "close" | "close-panel" | "cancel" | "dismiss-error" => Self::Close,
            "search" => Self::Search,
            "table" => Self::Table,
            "ddl" | "sql" | "schema" => Self::Code,
            "image" => Self::Image,
            "audio" => Self::Audio,
            "preview-cell" => Self::ExternalLink,
            "light" => Self::Sun,
            "dark" | "theme" => Self::Moon,
            "move-left" => Self::MoveLeft,
            "move-right" => Self::MoveRight,
            "expand" => Self::Expand,
            "minimize" => Self::Minimize,
            "columns" => Self::Columns,
            "load-range" => Self::RowRange,
            "all" | "load-all" => Self::AllRows,
            "select-fields" | "select-all" => Self::Check,
            "save" | "audio-save" | "save-cell" => Self::Export,
            command if command.starts_with("import-") => Self::Import,
            command if command.starts_with("date-") => Self::Calendar,
            command if command.starts_with("copy") => Self::Copy,
            command if command.starts_with("export") => Self::Export,
            _ => return None,
        })
    }

    fn svg(self) -> &'static str {
        match self {
            Self::Open => include_str!("../packaging/assets/commands/open.svg"),
            Self::Folder => include_str!("../packaging/assets/commands/folder.svg"),
            Self::Import => include_str!("../packaging/assets/commands/import.svg"),
            Self::Filter => include_str!("../packaging/assets/commands/filter.svg"),
            Self::Execute => include_str!("../packaging/assets/commands/execute.svg"),
            Self::Clear => include_str!("../packaging/assets/commands/clear.svg"),
            Self::Fields => include_str!("../packaging/assets/commands/fields.svg"),
            Self::Export => include_str!("../packaging/assets/commands/export.svg"),
            Self::Reload => include_str!("../packaging/assets/commands/reload.svg"),
            Self::Settings => include_str!("../packaging/assets/commands/settings.svg"),
            Self::Info => include_str!("../packaging/assets/commands/info.svg"),
            Self::Freeze => include_str!("../packaging/assets/commands/freeze.svg"),
            Self::Unfreeze => include_str!("../packaging/assets/commands/unfreeze.svg"),
            Self::Wrap => include_str!("../packaging/assets/commands/wrap.svg"),
            Self::Copy => include_str!("../packaging/assets/commands/copy.svg"),
            Self::Play => include_str!("../packaging/assets/commands/play.svg"),
            Self::Pause => include_str!("../packaging/assets/commands/pause.svg"),
            Self::Stop => include_str!("../packaging/assets/commands/stop.svg"),
            Self::Next => include_str!("../packaging/assets/commands/next.svg"),
            Self::Back => include_str!("../packaging/assets/commands/back.svg"),
            Self::Close => include_str!("../packaging/assets/commands/close.svg"),
            Self::Search => include_str!("../packaging/assets/commands/search.svg"),
            Self::Table => include_str!("../packaging/assets/commands/table.svg"),
            Self::Code => include_str!("../packaging/assets/commands/code.svg"),
            Self::Image => include_str!("../packaging/assets/commands/image.svg"),
            Self::Audio => include_str!("../packaging/assets/commands/audio.svg"),
            Self::Check => include_str!("../packaging/assets/commands/check.svg"),
            Self::ExternalLink => include_str!("../packaging/assets/commands/external-link.svg"),
            Self::Sun => include_str!("../packaging/assets/commands/sun.svg"),
            Self::Moon => include_str!("../packaging/assets/commands/moon.svg"),
            Self::MoveLeft => include_str!("../packaging/assets/commands/move-left.svg"),
            Self::MoveRight => include_str!("../packaging/assets/commands/move-right.svg"),
            Self::MoveUp => include_str!("../packaging/assets/commands/move-up.svg"),
            Self::MoveDown => include_str!("../packaging/assets/commands/move-down.svg"),
            Self::Expand => include_str!("../packaging/assets/commands/expand.svg"),
            Self::Columns => include_str!("../packaging/assets/commands/columns.svg"),
            Self::RowRange => include_str!("../packaging/assets/commands/row-range.svg"),
            Self::AllRows => include_str!("../packaging/assets/commands/all-rows.svg"),
            Self::Calendar => include_str!("../packaging/assets/commands/calendar.svg"),
            Self::ReadOnly => include_str!("../packaging/assets/commands/read-only.svg"),
            Self::Minimize => include_str!("../packaging/assets/commands/minimize.svg"),
            Self::Statistics => include_str!("../packaging/assets/commands/statistics.svg"),
        }
    }

    /// Shared encoded SVG in the normal light/dark foreground color.
    pub fn image(self, dark: bool) -> Arc<Image> {
        self.image_variant(usize::from(dark))
    }

    /// Each glyph has three cached variants: light, dark, and the white
    /// foreground shared by filled primary actions in both themes.
    fn image_variant(self, variant: usize) -> Arc<Image> {
        static CACHE: OnceLock<Vec<[Arc<Image>; 3]>> = OnceLock::new();
        CACHE.get_or_init(|| {
            let light = Colors::new(false);
            let dark = Colors::new(true);
            let foregrounds = [light.text, dark.text, light.on_primary];
            Icon::ALL
                .iter()
                .map(|icon| {
                    std::array::from_fn(|variant| {
                        let color = format!("#{:06X}", foregrounds[variant]);
                        Arc::new(Image::from_bytes(
                            ImageFormat::Svg,
                            icon.svg().replace("currentColor", &color).into_bytes(),
                        ))
                    })
                })
                .collect()
        })[self as usize][variant]
            .clone()
    }
}

/// A decorative glyph. Keep a visible label or a tooltip on its owning control.
pub fn glyph(icon: Icon, size: f32, dark: bool) -> Img {
    img(icon.image(dark))
        .size(px(size))
        .object_fit(ObjectFit::Contain)
        .flex_shrink_0()
}

/// White foreground for a filled primary action; keep its visible command label.
pub fn primary_glyph(icon: Icon, size: f32) -> Img {
    img(icon.image_variant(2))
        .size(px(size))
        .object_fit(ObjectFit::Contain)
        .flex_shrink_0()
}

/// A decorative 17px selection indicator. The enclosing control owns its
/// visible label, tooltip, enabled state and click/keyboard behavior.
pub fn checkbox(checked: bool, dark: bool) -> gpui::Div {
    let colors = Colors::new(dark);
    div()
        .size(px(17.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(if checked {
            colors.primary
        } else {
            colors.input_border
        }))
        .bg(rgb(if checked {
            colors.primary
        } else {
            colors.input
        }))
        .when(checked, |indicator| {
            indicator.child(primary_glyph(Icon::Check, 14.))
        })
}

/// Add to a stateful control using `.tooltip(icons::tooltip(label, dark))`.
/// The caller supplies already-localized text and may include its shortcut.
pub fn tooltip(
    label: impl Into<SharedString>,
    dark: bool,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let label = label.into();
    move |_, cx| {
        cx.new(|_| IconTooltip {
            label: label.clone(),
            dark,
        })
        .into()
    }
}

struct IconTooltip {
    label: SharedString,
    dark: bool,
}

impl Render for IconTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::new(self.dark);
        div()
            .px_2()
            .py_1()
            .max_w(px(340.))
            .rounded_md()
            .border_1()
            .border_color(rgb(colors.border))
            .bg(rgb(colors.surface))
            .text_color(rgb(colors.text))
            .text_size(px(12.))
            .shadow_md()
            .child(self.label.clone())
    }
}
