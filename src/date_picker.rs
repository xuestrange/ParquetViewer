//! Small, local calendar for filter inputs. It never reads dataset values.
//! The owning filter editor combines a selected date with any existing time,
//! fractional seconds and zone; this component only emits a calendar date.

use crate::controls::{self, ButtonStyle};
use chrono::{Datelike, Days, Local, Months, NaiveDate};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render,
    Subscription, Window, div, prelude::*, px, rgb,
};

use crate::{
    i18n::t,
    icons::{self, Icon, glyph},
    text_input::{InputEvent, TextInput},
    theme::Colors,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatePickerEvent {
    Select(NaiveDate),
    Cancel,
}

pub struct DatePicker {
    month: NaiveDate,
    selected: Option<NaiveDate>,
    cursor: NaiveDate,
    today: NaiveDate,
    dark: bool,
    focus: FocusHandle,
    year_editor: Option<YearEditor>,
}

struct YearEditor {
    input: Entity<TextInput>,
    _subscription: Subscription,
}

impl EventEmitter<DatePickerEvent> for DatePicker {}

impl DatePicker {
    pub fn new(value: &str, seed: Option<NaiveDate>, dark: bool, cx: &mut Context<Self>) -> Self {
        let today = Local::now().date_naive();
        let (selected, cursor) = initial_dates(value, seed, today);
        Self {
            // Every valid date has a valid first day in the same month.
            month: cursor.with_day(1).unwrap(),
            selected,
            cursor,
            today,
            dark,
            focus: cx.focus_handle(),
            year_editor: None,
        }
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        if let Some(editor) = &self.year_editor {
            editor
                .input
                .update(cx, |input, cx| input.set_dark(dark, cx));
        }
        cx.notify();
    }

    pub fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.year_editor
            .as_ref()
            .is_some_and(|editor| editor.input.focus_handle(cx).is_focused(window))
    }

    fn move_month(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(month) = shift_month(self.month, forward) {
            self.show_month(month, cx);
        }
    }

    fn show_month(&mut self, month: NaiveDate, cx: &mut Context<Self>) {
        self.month = month;
        self.cursor = month
            .with_day(self.cursor.day().min(month_days(month)))
            .unwrap();
        cx.notify();
    }

    fn edit_year(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(self.dark)
                .with_placeholder(&self.month.format("%Y").to_string())
        });
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::Changed => cx.notify(),
                InputEvent::Submit => this.apply_year(window, cx),
            });
        input.focus_handle(cx).focus(window);
        self.year_editor = Some(YearEditor {
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn edited_month(&self, cx: &App) -> Option<NaiveDate> {
        let text = self.year_editor.as_ref()?.input.read(cx).text();
        self.month.with_year(text.trim().parse().ok()?)
    }

    fn apply_year(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(month) = self.edited_month(cx) {
            self.year_editor = None;
            self.focus.focus(window);
            self.show_month(month, cx);
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.year_editor.take().is_some() {
            self.focus.focus(window);
            cx.notify();
        } else {
            cx.emit(DatePickerEvent::Cancel);
        }
    }

    fn move_cursor(&mut self, forward: bool, days: u64, cx: &mut Context<Self>) {
        let next = if forward {
            self.cursor.checked_add_days(Days::new(days))
        } else {
            self.cursor.checked_sub_days(Days::new(days))
        };
        if let Some(next) = next {
            self.cursor = next;
            self.month = next.with_day(1).unwrap();
            cx.notify();
        }
    }

    fn icon_button(
        &self,
        id: &'static str,
        label: &'static str,
        icon: Icon,
        enabled: bool,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = Colors::new(self.dark);
        controls::icon_button(id, colors, enabled)
            .child(glyph(icon, 13., self.dark))
            .tooltip(icons::tooltip(t(label), self.dark))
            .when(enabled, |button| {
                button.on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
            })
    }

    fn render_month_heading(&self, colors: Colors, cx: &Context<Self>) -> gpui::Div {
        let heading = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .gap_1()
            .font_weight(gpui::FontWeight::SEMIBOLD);
        if let Some(editor) = &self.year_editor {
            heading
                .child(div().w(px(76.)).child(editor.input.clone()))
                .child(self.month.format("-%m").to_string())
                .child(self.icon_button(
                    "calendar-apply-year",
                    "Go to year",
                    Icon::Check,
                    self.edited_month(cx).is_some(),
                    Self::apply_year,
                    cx,
                ))
        } else {
            heading.child(
                controls::button("calendar-edit-year", colors, ButtonStyle::Quiet, true)
                    .tooltip(icons::tooltip(t("Change year"), self.dark))
                    .child(self.month.format("%Y-%m").to_string())
                    .child(controls::chevron(colors, false))
                    .on_click(cx.listener(|this, _, window, cx| this.edit_year(window, cx))),
            )
        }
    }
}

impl Focusable for DatePicker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DatePicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::new(self.dark);
        let cells = month_grid(self.month);
        div()
            .id("filter-date-picker")
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .py_2()
            .border_t_1()
            .border_b_1()
            .border_color(rgb(colors.border))
            .bg(rgb(colors.surface))
            .text_color(rgb(colors.text))
            .text_size(px(11.))
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &crate::app::Dismiss, window, cx| {
                this.dismiss(window, cx);
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.year_editor.is_some() && event.keystroke.key != "escape" {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" => this.dismiss(window, cx),
                    "enter" => cx.emit(DatePickerEvent::Select(this.cursor)),
                    "left" => this.move_cursor(false, 1, cx),
                    "right" => this.move_cursor(true, 1, cx),
                    "up" => this.move_cursor(false, 7, cx),
                    "down" => this.move_cursor(true, 7, cx),
                    "y" if !event.keystroke.modifiers.modified() => this.edit_year(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.icon_button(
                        "calendar-previous",
                        "Previous month",
                        Icon::MoveLeft,
                        self.year_editor.is_none() && shift_month(self.month, false).is_some(),
                        |this, _, cx| this.move_month(false, cx),
                        cx,
                    ))
                    .child(self.render_month_heading(colors, cx))
                    .child(self.icon_button(
                        "calendar-next",
                        "Next month",
                        Icon::MoveRight,
                        self.year_editor.is_none() && shift_month(self.month, true).is_some(),
                        |this, _, cx| this.move_month(true, cx),
                        cx,
                    ))
                    .child(self.icon_button(
                        "calendar-close",
                        "Close calendar",
                        Icon::Close,
                        true,
                        |_, _, cx| cx.emit(DatePickerEvent::Cancel),
                        cx,
                    )),
            )
            .child(div().flex().gap_1().children(
                ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].map(|day| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(colors.muted))
                        .child(t(day))
                }),
            ))
            .children(cells.chunks(7).enumerate().map(|(week, dates)| {
                div().id(("calendar-week", week)).flex().gap_1().children(
                    dates.iter().enumerate().map(|(column, date)| {
                        let date = *date;
                        div()
                            .id(("calendar-day", week * 7 + column))
                            .flex_1()
                            .min_w_0()
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.))
                            .border_1()
                            .border_color(rgb(colors.surface))
                            .when_some(date, |button, date| {
                                button
                                    .child(date.day().to_string())
                                    .when(Some(date) == self.selected, |button| {
                                        button
                                            .bg(rgb(colors.primary))
                                            .text_color(rgb(colors.on_primary))
                                    })
                                    .when(
                                        date == self.today && Some(date) != self.selected,
                                        |button| {
                                            button
                                                .text_color(rgb(colors.accent))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                        },
                                    )
                                    .when(date == self.cursor, |button| {
                                        button.border_color(rgb(colors.accent))
                                    })
                                    .cursor_pointer()
                                    .hover(|button| {
                                        if Some(date) == self.selected {
                                            button.bg(rgb(colors.primary_hover))
                                        } else {
                                            button.bg(rgb(colors.hover))
                                        }
                                    })
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(DatePickerEvent::Select(date))
                                    }))
                            })
                    }),
                )
            }))
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pt_1()
                    .child(
                        controls::button("calendar-today", colors, ButtonStyle::Standard, true)
                            .child(t("Today"))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(DatePickerEvent::Select(Local::now().date_naive()));
                            })),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_size(px(10.))
                            .text_color(rgb(colors.muted))
                            .child(t("YYYY-MM-DD")),
                    ),
            )
    }
}

/// Returns the date part without interpreting or changing any time/zone suffix.
fn initial_dates(
    value: &str,
    seed: Option<NaiveDate>,
    today: NaiveDate,
) -> (Option<NaiveDate>, NaiveDate) {
    let value = value.trim();
    let date = value.split(['T', 't', ' ']).next().unwrap_or(value);
    let selected = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok();
    (selected, selected.or(seed).unwrap_or(today))
}

fn shift_month(first: NaiveDate, forward: bool) -> Option<NaiveDate> {
    if forward {
        first.checked_add_months(Months::new(1))
    } else {
        first.checked_sub_months(Months::new(1))
    }
}

fn month_days(first: NaiveDate) -> u32 {
    (28..=31)
        .rev()
        .find(|day| first.with_day(*day).is_some())
        .unwrap()
}

/// Only the current month's dates are interactive; padding cells remain empty.
/// No date arithmetic outside chrono's supported year range is required.
fn month_grid(first: NaiveDate) -> Vec<Option<NaiveDate>> {
    let leading = first.weekday().num_days_from_monday() as usize;
    let days = month_days(first) as usize;
    let size = (leading + days).div_ceil(7) * 7;
    (0..size)
        .map(|index| {
            (index >= leading && index < leading + days)
                .then(|| first.with_day((index - leading + 1) as u32).unwrap())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_grid_uses_monday_first_and_exact_leap_year_days() {
        let february = month_grid(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap());
        assert_eq!(february.len(), 35);
        assert!(february[..3].iter().all(Option::is_none));
        let dates = february.into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(dates.len(), 29);
        assert_eq!(dates[0].day(), 1);
        assert_eq!(dates[28].day(), 29);
        let ordinary = month_grid(NaiveDate::from_ymd_opt(2100, 2, 1).unwrap());
        assert_eq!(ordinary.into_iter().flatten().count(), 28);
        let monday = month_grid(NaiveDate::from_ymd_opt(2021, 2, 1).unwrap());
        assert_eq!(monday.len(), 28);
        assert_eq!(monday[0].unwrap().day(), 1);
    }

    #[test]
    fn calendar_initial_selection_reads_date_without_interpreting_time_zone() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let seed = NaiveDate::from_ymd_opt(1982, 3, 17).unwrap();
        for value in [
            "2024-02-29",
            "2024-02-29 13:14:15.123456789+08:00",
            "2024-02-29T13:14:15Z",
            "2024-02-29t13:14:15.123456789+08:00",
            " 2024-02-29 ",
        ] {
            assert_eq!(
                initial_dates(value, Some(seed), today),
                (Some(expected), expected)
            );
        }
        for value in ["", "2023-02-29", "not a date", "2024-99-99", "😀"] {
            assert_eq!(initial_dates(value, Some(seed), today), (None, seed));
            assert_eq!(initial_dates(value, None, today), (None, today));
        }
    }

    #[test]
    fn month_navigation_crosses_years_and_stops_at_chrono_bounds() {
        let december = NaiveDate::from_ymd_opt(2024, 12, 1).unwrap();
        let january = NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
        assert_eq!(shift_month(december, true), Some(january));
        assert_eq!(shift_month(january, false), Some(december));
        let minimum = NaiveDate::MIN.with_day(1).unwrap();
        let maximum = NaiveDate::MAX.with_day(1).unwrap();
        assert!(shift_month(minimum, false).is_none());
        assert!(shift_month(maximum, true).is_none());
        assert_eq!(
            month_grid(minimum).into_iter().flatten().count(),
            month_days(minimum) as usize
        );
        assert_eq!(
            month_grid(maximum).into_iter().flatten().count(),
            month_days(maximum) as usize
        );
    }
}
