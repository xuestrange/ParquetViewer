//! Shared styling for interactive controls. Callers supply content and behavior.
use crate::theme::Colors;
use gpui::{Div, ElementId, FontWeight, Stateful, div, prelude::*, px, rgb, rgba};

pub(crate) const HEIGHT: f32 = 28.;
const RADIUS: f32 = 5.;
const MENU_PADDING: f32 = 4.;
pub(crate) const MENU_VERTICAL_INSET: f32 = 2. * (MENU_PADDING + 1.);

/// Keep the axis on screen and construct only the bars inside the chart viewport.
pub(crate) fn chart_rows(
    id: impl Into<ElementId>,
    count: usize,
    render_row: impl Fn(usize) -> Stateful<Div> + 'static,
) -> gpui::UniformList {
    const ROW_HEIGHT: f32 = 20.;
    gpui::uniform_list(id, count, move |range, _, _| {
        range.map(|row| render_row(row).h(px(ROW_HEIGHT))).collect()
    })
    .w_full()
    .h(px(ROW_HEIGHT * count.min(12) as f32))
    .flex_shrink_0()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonStyle {
    Standard,
    Primary,
    Quiet,
}

pub(crate) fn button(
    id: impl Into<ElementId>,
    c: Colors,
    style: ButtonStyle,
    enabled: bool,
) -> Stateful<Div> {
    let (foreground, fill, border, hover, pressed) = match style {
        ButtonStyle::Standard => (c.text, rgb(c.surface), rgb(c.border), c.hover, c.selected),
        ButtonStyle::Primary => (
            c.on_primary,
            rgb(c.primary),
            rgb(c.primary),
            c.primary_hover,
            c.primary_hover,
        ),
        ButtonStyle::Quiet => (c.text, rgba(0), rgba(0), c.hover, c.selected),
    };
    div()
        .id(id)
        .h(px(HEIGHT))
        .px(px(9.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(5.))
        .whitespace_nowrap()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .border_1()
        .rounded(px(RADIUS))
        .border_color(border)
        .bg(fill)
        .text_color(rgb(foreground))
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|el| el.bg(rgb(hover)))
                .active(|el| el.bg(rgb(pressed)))
                .focus(|el| el.border_color(rgb(c.accent)))
        })
        .when(!enabled, |el| el.opacity(0.48))
}

pub(crate) fn icon_button(id: impl Into<ElementId>, c: Colors, enabled: bool) -> Stateful<Div> {
    button(id, c, ButtonStyle::Quiet, enabled)
        .size(px(HEIGHT))
        .px_0()
}

pub(crate) fn checkbox(
    id: impl Into<ElementId>,
    c: Colors,
    dark: bool,
    checked: bool,
    enabled: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(6.))
        .text_size(px(12.))
        .text_color(rgb(c.text))
        .whitespace_nowrap()
        .when(enabled, |el| {
            el.cursor_pointer().hover(|el| el.text_color(rgb(c.accent)))
        })
        .when(!enabled, |el| el.opacity(0.48))
        .child(crate::icons::checkbox(checked, dark))
}

pub(crate) fn field(c: Colors, focused: bool) -> Div {
    div()
        .rounded(px(RADIUS))
        .border_1()
        .border_color(rgb(if focused { c.accent } else { c.input_border }))
        .bg(rgb(c.input))
        .text_color(rgb(c.text))
        .text_size(px(12.))
}

pub(crate) fn selector(
    id: impl Into<ElementId>,
    c: Colors,
    open: bool,
    enabled: bool,
) -> Stateful<Div> {
    field(c, open)
        .id(id)
        .h(px(HEIGHT))
        .min_w_0()
        .px_2()
        .flex()
        .items_center()
        .gap(px(6.))
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|el| el.border_color(rgb(c.accent)))
                .focus(|el| el.border_color(rgb(c.accent)))
        })
        .when(!enabled, |el| el.opacity(0.48))
}

pub(crate) fn chevron(c: Colors, open: bool) -> Div {
    div()
        .flex_shrink_0()
        .text_size(px(11.))
        .text_color(rgb(c.muted))
        .child(if open { "▴" } else { "▾" })
}

pub(crate) fn segment_group(c: Colors) -> Div {
    div()
        .flex()
        .min_w_0()
        .items_center()
        .gap(px(2.))
        .p(px(2.))
        .border_1()
        .rounded(px(7.))
        .border_color(rgb(c.border))
        .bg(rgb(c.bar))
}

pub(crate) fn segment(id: impl Into<ElementId>, c: Colors, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(24.))
        .px(px(7.))
        .min_w_0()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(4.))
        .rounded(px(4.))
        .border_1()
        .whitespace_nowrap()
        .text_size(px(12.))
        .border_color(if selected { rgb(c.border) } else { rgba(0) })
        .when(selected, |el| {
            el.bg(rgb(c.surface)).font_weight(FontWeight::SEMIBOLD)
        })
        .text_color(rgb(if selected { c.accent } else { c.text }))
        .cursor_pointer()
        .when(!selected, |el| el.hover(|el| el.bg(rgb(c.hover))))
}

pub(crate) fn menu_surface(id: impl Into<ElementId>, c: Colors) -> Stateful<Div> {
    div()
        .id(id)
        .p(px(MENU_PADDING))
        .rounded(px(7.))
        .border_1()
        .border_color(rgb(c.border))
        .bg(rgb(c.surface))
        .text_color(rgb(c.text))
        .shadow_md()
}

pub(crate) fn menu_item(
    id: impl Into<ElementId>,
    c: Colors,
    selected: bool,
    enabled: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h(px(28.))
        .px_2()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .rounded(px(4.))
        .text_size(px(12.))
        .text_color(rgb(if selected { c.accent } else { c.text }))
        .when(selected, |el| el.bg(rgb(c.selected)))
        .when(enabled, |el| {
            el.cursor_pointer().hover(|el| el.bg(rgb(c.hover)))
        })
        .when(!enabled, |el| el.opacity(0.48))
}
