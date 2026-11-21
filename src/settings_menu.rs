//! Application preferences.
use super::*;
use crate::profile_stats::DEFAULT_CATEGORY_CHART_BARS;

pub(super) const DEFAULT_PROFILE_CHART_MAX_ROWS: usize = 2_000_000;

#[derive(Clone, Copy)]
enum ChartScale {
    Rows,
    Bars,
}

pub(super) fn limit_text(limit: usize) -> String {
    if limit == usize::MAX {
        "INF".into()
    } else {
        limit.to_string()
    }
}

fn parse_limit(value: &str) -> Option<usize> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("inf") || value == "∞" {
        Some(usize::MAX)
    } else {
        value.parse().ok()
    }
}

impl ChartScale {
    fn stops(self) -> [usize; 5] {
        match self {
            Self::Rows => [0, 100_000, 1_000_000, 10_000_000, usize::MAX],
            Self::Bars => [0, 8, 32, 100, usize::MAX],
        }
    }

    fn labels(self) -> [&'static str; 5] {
        match self {
            Self::Rows => ["0", "100K", "1M", "10M", "INF"],
            Self::Bars => ["0", "8", "32", "100", "INF"],
        }
    }

    // The last quarter approaches infinity continuously. The thumb and pointer
    // share this mapping, so exact typed values also land on the correct scale.
    fn value_at(self, position: f64) -> usize {
        let scaled = position.clamp(0., 1.) * 4.;
        let segment = scaled.floor() as usize;
        let stops = self.stops();
        if segment == 4 {
            return usize::MAX;
        }
        let low = stops[segment] as f64;
        let part = scaled.fract();
        let value = if segment == 3 {
            low / (1. - part)
        } else {
            low + part * (stops[segment + 1] as f64 - low)
        };
        (value.round() as usize).min(usize::MAX - 1)
    }

    fn position(self, value: usize) -> f64 {
        if value == usize::MAX {
            return 1.;
        }
        let stops = self.stops();
        let segment = stops.partition_point(|&stop| stop <= value) - 1;
        let part = if segment == 3 {
            1. - stops[3] as f64 / value as f64
        } else {
            (value - stops[segment]) as f64 / (stops[segment + 1] - stops[segment]) as f64
        };
        (segment as f64 + part) / 4.
    }

    fn set_from_pointer(
        self,
        input: &Entity<TextInput>,
        bounds: Bounds<Pixels>,
        x: Pixels,
        cx: &mut App,
    ) {
        let track_width = f32::from(bounds.size.width) - 2. * SLIDER_INSET;
        if track_width > 0. {
            let position = (f32::from(x - bounds.left()) - SLIDER_INSET) / track_width;
            let value = self.value_at(f64::from(position));
            input.update(cx, |field, cx| field.set_text(&limit_text(value), cx));
        }
    }
}

const SLIDER_INSET: f32 = 8.;

#[derive(Clone, Copy)]
enum SettingCommand {
    Theme(ThemeMode),
    Language(Language),
    ProfileChartLimit,
    About,
}

enum SettingEntry {
    Heading(&'static str),
    Action {
        label: &'static str,
        icon: Icon,
        command: SettingCommand,
        enabled: bool,
        checked: bool,
    },
    Separator,
}

impl SettingEntry {
    fn choice(label: &'static str, icon: Icon, command: SettingCommand, checked: bool) -> Self {
        Self::Action {
            label,
            icon,
            command,
            enabled: true,
            checked,
        }
    }

    fn action(label: &'static str, icon: Icon, command: SettingCommand, enabled: bool) -> Self {
        Self::Action {
            label,
            icon,
            command,
            enabled,
            checked: false,
        }
    }

    fn height(&self) -> f32 {
        match self {
            Self::Heading(_) => 25.,
            Self::Action { .. } => controls::HEIGHT,
            Self::Separator => 9.,
        }
    }
}

impl Viewer {
    fn run_setting_command(&mut self, command: SettingCommand, cx: &mut Context<Self>) {
        self.settings_menu = None;
        match command {
            SettingCommand::Theme(mode) => self.set_theme_mode(mode, cx),
            SettingCommand::Language(language) => self.set_language(language, cx),
            SettingCommand::ProfileChartLimit => self.show_panel(Panel::ProfileChartLimit, cx),
            SettingCommand::About => self.show_panel(Panel::About, cx),
        }
        cx.notify();
    }

    pub(super) fn render_settings_menu(
        &self,
        window: &Window,
        toolbar_height: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.settings_menu.is_none() {
            return div().into_any_element();
        }
        let c = Colors::new(self.preferences.dark);
        let idle = self.busy.is_none() && self.panel.is_none();
        let entries = vec![
            SettingEntry::Heading("Theme"),
            SettingEntry::choice(
                "Light",
                Icon::Sun,
                SettingCommand::Theme(ThemeMode::Light),
                self.preferences.theme_mode == ThemeMode::Light,
            ),
            SettingEntry::choice(
                "Dark",
                Icon::Moon,
                SettingCommand::Theme(ThemeMode::Dark),
                self.preferences.theme_mode == ThemeMode::Dark,
            ),
            SettingEntry::choice(
                "Follow System",
                Icon::Settings,
                SettingCommand::Theme(ThemeMode::System),
                self.preferences.theme_mode == ThemeMode::System,
            ),
            SettingEntry::Heading("Language"),
            SettingEntry::choice(
                "English",
                Icon::Settings,
                SettingCommand::Language(Language::English),
                self.preferences.language == Language::English,
            ),
            SettingEntry::choice(
                "简体中文",
                Icon::Settings,
                SettingCommand::Language(Language::SimplifiedChinese),
                self.preferences.language == Language::SimplifiedChinese,
            ),
            SettingEntry::Heading("Display"),
            SettingEntry::action(
                "Chart Data Limits",
                Icon::Statistics,
                SettingCommand::ProfileChartLimit,
                idle,
            ),
            SettingEntry::Separator,
            SettingEntry::action(
                "About Parquet Viewer",
                Icon::Info,
                SettingCommand::About,
                idle,
            ),
        ];
        let viewport = window.viewport_size();
        let width = 300_f32.min((f32::from(viewport.width) - 16.).max(160.));
        let desired =
            controls::MENU_VERTICAL_INSET + entries.iter().map(SettingEntry::height).sum::<f32>();
        let height = desired.min((f32::from(viewport.height) - toolbar_height - 8.).max(30.));
        div()
            .id("settings-menu-backdrop")
            .occlude()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.settings_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.settings_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                controls::menu_surface("settings-menu", c)
                    .absolute()
                    .left(px(8. + window_chrome::leading_inset(window)))
                    .top(px(toolbar_height))
                    .w(px(width))
                    .h(px(height))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(entries.into_iter().enumerate().map(|(index, entry)| {
                        match entry {
                            SettingEntry::Heading(label) => div()
                                .h(px(25.))
                                .flex_shrink_0()
                                .px_3()
                                .flex()
                                .items_center()
                                .text_size(px(10.))
                                .text_color(rgb(c.muted))
                                .child(t(label))
                                .into_any_element(),
                            SettingEntry::Separator => div()
                                .h(px(9.))
                                .flex_shrink_0()
                                .px_2()
                                .flex()
                                .items_center()
                                .child(div().w_full().h(px(1.)).bg(rgb(c.grid)))
                                .into_any_element(),
                            SettingEntry::Action {
                                label,
                                icon,
                                command,
                                enabled,
                                checked,
                            } => {
                                controls::menu_item(("setting-command", index), c, checked, enabled)
                                    .child(icons::glyph(icon, 14., self.preferences.dark))
                                    .child(div().flex_1().min_w_0().truncate().child(t(label)))
                                    .when(checked, |row| {
                                        row.child(icons::glyph(
                                            Icon::Check,
                                            14.,
                                            self.preferences.dark,
                                        ))
                                    })
                                    .tooltip(icons::tooltip(t(label), self.preferences.dark))
                                    .when(enabled, |row| {
                                        row.on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.run_setting_command(command, cx);
                                        }))
                                    })
                                    .into_any_element()
                            }
                        }
                    })),
            )
            .into_any_element()
    }

    pub(super) fn apply_profile_chart_limit(&mut self, cx: &mut Context<Self>) {
        let (Some(rows), Some(category_bars)) = (
            parse_limit(&self.profile_chart_limit.read(cx).text()),
            parse_limit(&self.profile_chart_category_limit.read(cx).text()),
        ) else {
            return;
        };
        let rows = (rows != self.preferences.profile_chart_max_rows).then_some(rows);
        let category_bars = (category_bars != self.preferences.profile_chart_category_bars)
            .then_some(category_bars);
        self.panel = None;
        if rows.is_some() || category_bars.is_some() {
            self.update_preferences(
                PreferenceUpdate::ProfileChartLimits {
                    rows,
                    category_bars,
                },
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn sync_chart_limit_inputs(
        &self,
        preferences: &Preferences,
        cx: &mut Context<Self>,
    ) {
        if self.panel != Some(Panel::ProfileChartLimit) {
            return;
        }
        for (input, old, new) in [
            (
                &self.profile_chart_limit,
                self.preferences.profile_chart_max_rows,
                preferences.profile_chart_max_rows,
            ),
            (
                &self.profile_chart_category_limit,
                self.preferences.profile_chart_category_bars,
                preferences.profile_chart_category_bars,
            ),
        ] {
            if old != new && parse_limit(&input.read(cx).text()) == Some(old) {
                input.update(cx, |input, cx| input.set_text(&limit_text(new), cx));
            }
        }
    }

    pub(super) fn render_profile_chart_limit(&self, cx: &mut Context<Self>) -> gpui::Div {
        let c = Colors::new(self.preferences.dark);
        let rows = parse_limit(&self.profile_chart_limit.read(cx).text());
        let bars = parse_limit(&self.profile_chart_category_limit.read(cx).text());
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.chart_limit_slider(ChartScale::Rows, rows, c))
            .child(self.chart_limit_slider(ChartScale::Bars, bars, c))
            .child(div().flex().flex_wrap().gap_2().children([
                self.button(
                    "apply-profile-chart-limit",
                    "Apply",
                    rows.is_some() && bars.is_some(),
                    |this, _, cx| this.apply_profile_chart_limit(cx),
                    cx,
                ),
                self.button(
                    "reset-profile-chart-limit",
                    "Use Defaults",
                    true,
                    |this, window, cx| {
                        this.profile_chart_limit.update(cx, |input, cx| {
                            input.set_text(&DEFAULT_PROFILE_CHART_MAX_ROWS.to_string(), cx)
                        });
                        this.profile_chart_category_limit.update(cx, |input, cx| {
                            input.set_text(&DEFAULT_CATEGORY_CHART_BARS.to_string(), cx)
                        });
                        this.profile_chart_limit.focus_handle(cx).focus(window);
                    },
                    cx,
                ),
            ]))
    }

    fn chart_limit_slider(&self, scale: ChartScale, value: Option<usize>, c: Colors) -> gpui::Div {
        let (input, title, help, id) = match scale {
            ChartScale::Rows => (
                &self.profile_chart_limit,
                "Maximum result rows for statistics charts",
                "Above the limit, statistics still run. Choose 0 to turn off charts or INF for all rows.",
                "chart-row-slider",
            ),
            ChartScale::Bars => (
                &self.profile_chart_category_limit,
                "Maximum bars per chart",
                "Choose 0 to hide bar charts or INF to show all bars.",
                "chart-bar-slider",
            ),
        };
        let digits = value.map_or(0, |limit| limit_text(limit).len());
        let input_width = (20. + 8. * digits as f32).max(104.);
        let geometry = ScrollHandle::new();
        let mouse_input = input.clone();
        let drag_input = input.clone();
        let key_input = input.clone();
        let track = div()
            .id(id)
            .track_scroll(&geometry)
            .focusable()
            .tab_stop(true)
            .cursor_pointer()
            .flex()
            .items_center()
            .px(px(SLIDER_INSET))
            .h(px(26.))
            .rounded(px(4.))
            .focus(|style| style.bg(rgb(c.accent_soft)))
            .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                scale.set_from_pointer(&mouse_input, geometry.bounds(), event.position.x, cx);
            })
            .on_drag(input.clone(), |_, _, _, cx| cx.new(|_| gpui::Empty))
            .on_drag_move(
                move |event: &gpui::DragMoveEvent<Entity<TextInput>>, _, cx| {
                    if event.drag(cx) == &drag_input {
                        scale.set_from_pointer(
                            &drag_input,
                            event.bounds,
                            event.event.position.x,
                            cx,
                        );
                    }
                },
            )
            .on_key_down(move |event, _, cx| {
                let stops = scale.stops();
                let current = parse_limit(&key_input.read(cx).text()).unwrap_or(0);
                let next = match event.keystroke.key.as_str() {
                    "home" => 0,
                    "end" => usize::MAX,
                    "left" | "down" => stops
                        .iter()
                        .rev()
                        .copied()
                        .find(|&v| v < current)
                        .unwrap_or(0),
                    "right" | "up" => stops
                        .iter()
                        .copied()
                        .find(|&v| v > current)
                        .unwrap_or(usize::MAX),
                    _ => return,
                };
                key_input.update(cx, |field, cx| field.set_text(&limit_text(next), cx));
                cx.stop_propagation();
            })
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(c.input_border))
                    .when_some(value, |rail, value| {
                        rail.child(
                            div()
                                .h_full()
                                .w(gpui::relative(scale.position(value) as f32))
                                .rounded_full()
                                .bg(rgb(c.primary)),
                        )
                    })
                    .children((0..5).map(|index| {
                        div()
                            .absolute()
                            .left(gpui::relative(index as f32 / 4.))
                            .ml(px(-2.))
                            .size(px(4.))
                            .rounded_full()
                            .bg(rgb(c.muted))
                    }))
                    .when_some(value, |rail, value| {
                        rail.child(
                            div()
                                .absolute()
                                .left(gpui::relative(scale.position(value) as f32))
                                .top(px(-5.))
                                .ml(px(-7.))
                                .size(px(14.))
                                .rounded_full()
                                .border_2()
                                .border_color(rgb(c.primary))
                                .bg(rgb(c.surface)),
                        )
                    }),
            );
        let labels = div().relative().mx(px(SLIDER_INSET)).h(px(18.)).children(
            scale
                .labels()
                .into_iter()
                .enumerate()
                .map(|(index, label)| {
                    let input = input.clone();
                    div()
                        .id((id, index))
                        .absolute()
                        .left(gpui::relative(index as f32 / 4.))
                        .w(px(42.))
                        .text_size(px(10.))
                        .text_color(rgb(c.muted))
                        .map(|el| match index {
                            0 => el.ml(px(-3.)),
                            4 => el.ml(px(-39.)).text_right(),
                            _ => el.ml(px(-21.)).text_center(),
                        })
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(c.accent)))
                        .on_click(move |_, _, cx| {
                            input.update(cx, |field, cx| {
                                field.set_text(&limit_text(scale.stops()[index]), cx)
                            });
                        })
                        .child(t(label))
                }),
        );
        div()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t(title)),
                    )
                    .child(self.input_shell(input, Some(input_width))),
            )
            .child(track)
            .child(labels)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(if value.is_some() { c.muted } else { c.danger }))
                    .child(t(if value.is_some() {
                        help
                    } else {
                        "Enter a nonnegative whole number or INF."
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_scales_match_ticks_and_exact_values_in_both_directions() {
        for scale in [ChartScale::Rows, ChartScale::Bars] {
            for (index, stop) in scale.stops().into_iter().enumerate() {
                let position = index as f64 / 4.;
                assert_eq!(scale.value_at(position), stop);
                assert_eq!(scale.position(stop), position);
            }
            let mut previous = 0;
            for pixel in 0..=500 {
                let value = scale.value_at(pixel as f64 / 500.);
                assert!(value >= previous);
                assert_eq!(parse_limit(&limit_text(value)), Some(value));
                assert_eq!(scale.value_at(scale.position(value)), value);
                previous = value;
            }
        }
        assert_eq!(parse_limit(" ∞ "), Some(usize::MAX));
        assert_eq!(parse_limit("Inf"), Some(usize::MAX));
        assert_eq!(parse_limit("-1"), None);
        assert_eq!(parse_limit("1.5"), None);
    }
}
