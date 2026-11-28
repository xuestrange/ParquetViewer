//! Direct data commands, settings, and file actions in the custom titlebar.
use super::*;

#[path = "file_menu.rs"]
mod file_menu;

const SECOND_ROW_HEIGHT: f32 = 30.;
const DRAG_REGION_MIN_WIDTH: f32 = 96.;
const COMMAND_GROUP_GAP: f32 = 12.;
const LABELS: [&str; 7] = [
    "Settings",
    "File",
    "Fields",
    "Filters",
    "Statistical Analysis",
    "First",
    "Last",
];

struct Command {
    id: &'static str,
    hint: &'static str,
    icon: Icon,
    enabled: bool,
    active: bool,
    handler: fn(&mut Viewer, &mut Window, &mut Context<Viewer>),
}

fn command_group() -> gpui::Div {
    div().flex().items_center().gap(px(3.)).flex_shrink_0()
}

/// Measure the persistent captions with GPUI's cached text shaper. Every
/// command keeps its icon and caption; narrow windows use a second short row.
fn command_widths(window: &Window) -> [f32; LABELS.len()] {
    LABELS.map(|label| {
        let label = t(label);
        let run = TextRun {
            len: label.len(),
            font: gpui::font("Helvetica Neue"),
            color: rgb(0).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // 13px icon, 4px gap, horizontal padding and a rounding allowance.
        26. + f32::from(
            window
                .text_system()
                .shape_line(label.into(), px(11.), &[run], None)
                .width,
        )
    })
}

fn needs_second_row(widths: &[f32; LABELS.len()], window: &Window) -> bool {
    // Padding, group gaps, native controls and the titlebar drag target.
    widths.iter().sum::<f32>()
        + 94.
        + COMMAND_GROUP_GAP
        + window_chrome::leading_inset(window)
        + DRAG_REGION_MIN_WIDTH
        > f32::from(window.viewport_size().width)
}

#[derive(Clone, Copy)]
pub(super) struct Layout {
    widths: [f32; LABELS.len()],
    stacked: bool,
    pub height: f32,
}

pub(super) fn layout(window: &Window) -> Layout {
    let widths = command_widths(window);
    let stacked = needs_second_row(&widths, window);
    Layout {
        widths,
        stacked,
        height: window_chrome::HEIGHT + if stacked { SECOND_ROW_HEIGHT } else { 0. },
    }
}

impl Viewer {
    fn compact_command_button(
        &self,
        command: Command,
        label: &'static str,
        width: f32,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.preferences.dark);
        let Command {
            id,
            hint,
            icon,
            enabled,
            active,
            handler,
            ..
        } = command;
        controls::button(id, c, ButtonStyle::Quiet, enabled)
            .h(px(24.))
            .w(px(width))
            .px_0()
            .gap(px(4.))
            .text_size(px(11.))
            .when(active, |button| {
                button.bg(rgb(c.accent_soft)).text_color(rgb(c.accent))
            })
            .child(icons::glyph(icon, 13., self.preferences.dark))
            .child(t(label))
            .tooltip(icons::tooltip(t(hint), self.preferences.dark))
            .when(enabled, |button| {
                button.on_click(cx.listener(move |this, _, window, cx| {
                    this.focus.focus(window);
                    if id != "settings" {
                        this.settings_menu = None;
                    }
                    if id != "file-menu-trigger" {
                        this.file_menu_open = false;
                    }
                    handler(this, window, cx);
                }))
            })
    }

    pub(super) fn render_commands(
        &mut self,
        window: &Window,
        layout: Layout,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.preferences.dark);
        let idle = self.busy.is_none() && self.panel.is_none();
        let ready = idle && !self.selecting_initial_fields;
        let loaded = self.engine.is_some();
        let navigable = idle
            && self
                .result
                .as_ref()
                .is_some_and(|result| !result.is_empty());
        let commands: [Command; LABELS.len()] = [
            Command {
                id: "settings",
                hint: "Settings",
                icon: Icon::Settings,
                enabled: ready,
                active: self.settings_menu.is_some(),
                handler: |this, window, cx| {
                    this.axis_menu = None;
                    this.selection_menu = None;
                    this.settings_menu = if this.settings_menu.is_some() {
                        None
                    } else {
                        Some(point(
                            px(8. + window_chrome::leading_inset(window)),
                            px(self::layout(window).height),
                        ))
                    };
                    cx.notify();
                },
            },
            Command {
                id: "file-menu-trigger",
                hint: "File",
                icon: Icon::Open,
                enabled: ready,
                active: self.file_menu_open,
                handler: |this, _, cx| {
                    this.axis_menu = None;
                    this.selection_menu = None;
                    this.file_menu_open = !this.file_menu_open;
                    cx.notify();
                },
            },
            Command {
                id: "fields",
                hint: "Choose Fields…    ⌘F",
                icon: Icon::Fields,
                enabled: loaded && idle,
                active: self.fields_sidebar_open,
                handler: |this, window, cx| this.toggle_fields_sidebar(window, cx),
            },
            Command {
                id: "compact-filters",
                hint: "Show or hide filters    ⌥⌘F",
                icon: Icon::Filter,
                enabled: loaded && ready,
                active: self.filter_sidebar_open,
                handler: |this, window, cx| this.toggle_filter_sidebar(window, cx),
            },
            Command {
                id: "statistical-analysis",
                hint: "Statistical Analysis",
                icon: Icon::Statistics,
                enabled: idle
                    && self
                        .result
                        .as_ref()
                        .is_some_and(|result| !result.columns.is_empty()),
                active: self.statistics_tab.is_some(),
                handler: |this, window, cx| this.toggle_statistics_sidebar(window, cx),
            },
            Command {
                id: "loaded-first",
                hint: if self.csv_browse.is_some() {
                    "Jump to the first source row"
                } else {
                    "Jump to first loaded row (current result only)"
                },
                icon: Icon::MoveUp,
                enabled: navigable,
                active: false,
                handler: |this, _, cx| this.jump_loaded_rows(false, cx),
            },
            Command {
                id: "loaded-last",
                hint: if self.csv_browse.is_some() {
                    "Jump to the last indexed row"
                } else {
                    "Jump to last loaded row (current result only)"
                },
                icon: Icon::MoveDown,
                enabled: navigable,
                active: false,
                handler: |this, _, cx| this.jump_loaded_rows(true, cx),
            },
        ];
        let Layout {
            widths, stacked, ..
        } = layout;
        let mut analysis = command_group();
        let mut loading = command_group();
        let mut left = div()
            .flex()
            .items_center()
            .gap(px(COMMAND_GROUP_GAP))
            .flex_shrink_0();
        for (index, (command, width)) in commands.into_iter().zip(widths).enumerate() {
            let button = self.compact_command_button(command, LABELS[index], width, cx);
            match index {
                0..=1 => left = left.child(button),
                2..=4 => analysis = analysis.child(button),
                _ => loading = loading.child(button),
            }
        }
        let top = |left: gpui::Div| {
            window_chrome::bar_with_tint(self.preferences.dark, window, self.window_color_index)
                .child(left)
                .child(window_chrome::drag_region().min_w(px(DRAG_REGION_MIN_WIDTH)))
        };
        let area = div()
            .id("command-area")
            .w_full()
            .h(px(layout.height))
            .flex_shrink_0()
            .flex()
            .flex_col();
        if stacked {
            area.child(top(left)).child(
                div()
                    .id("data-command-row")
                    .h(px(SECOND_ROW_HEIGHT))
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px_2()
                    .bg(rgb(theme::window_tint(
                        self.window_color_index,
                        self.preferences.dark,
                    )))
                    .border_b_1()
                    .border_color(rgb(c.border))
                    .occlude()
                    .child(analysis)
                    .child(loading),
            )
        } else {
            area.child(top(left.child(analysis).child(loading)))
        }
    }
}
