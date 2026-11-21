// Adapted from GPUI 0.2.2's examples/input.rs, Copyright Zed Industries.
// Original example: https://github.com/zed-industries/zed/blob/69e2130295c2649963eb639fc70b4f2ee8ea1624/crates/gpui/examples/input.rs
// Original code is licensed under Apache-2.0; see THIRD_PARTY_NOTICES.md.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollWheelEvent, ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle,
    Window, actions, div, fill, point, prelude::*, px, relative, rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::Colors;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        DocumentHome,
        DocumentEnd,
        SelectHome,
        SelectEnd,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
        Submit,
        Newline,
    ]
);

#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    Changed,
    Submit,
}

/// Install once during application startup. Context scoping keeps these shortcuts
/// from handling keyboard events when a table or the application owns focus.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("up", Up, Some("TextInput")),
        KeyBinding::new("down", Down, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("shift-up", SelectUp, Some("TextInput")),
        KeyBinding::new("shift-down", SelectDown, Some("TextInput")),
        KeyBinding::new("cmd-a", SelectAll, Some("TextInput")),
        KeyBinding::new("cmd-v", Paste, Some("TextInput")),
        KeyBinding::new("cmd-c", Copy, Some("TextInput")),
        KeyBinding::new("cmd-x", Cut, Some("TextInput")),
        KeyBinding::new("cmd-z", Undo, Some("TextInput")),
        KeyBinding::new("cmd-shift-z", Redo, Some("TextInput")),
        KeyBinding::new("home", DocumentHome, Some("TextInput")),
        KeyBinding::new("end", DocumentEnd, Some("TextInput")),
        KeyBinding::new("cmd-left", Home, Some("TextInput")),
        KeyBinding::new("cmd-right", End, Some("TextInput")),
        KeyBinding::new("cmd-up", DocumentHome, Some("TextInput")),
        KeyBinding::new("cmd-down", DocumentEnd, Some("TextInput")),
        KeyBinding::new("cmd-shift-left", SelectHome, Some("TextInput")),
        KeyBinding::new("cmd-shift-right", SelectEnd, Some("TextInput")),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some("TextInput")),
        KeyBinding::new("enter", Submit, Some("TextInput")),
        KeyBinding::new("shift-enter", Newline, Some("TextInput")),
    ]);
}

#[derive(Clone)]
struct Snapshot {
    content: SharedString,
    selection: Range<usize>,
    reversed: bool,
}

struct DisplayLine {
    start: usize,
    layout: ShapedLine,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    multiline: bool,
    dark: bool,
    fill_height: bool,
    read_only: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_lines: Vec<DisplayLine>,
    last_bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
    scroll: Point<Pixels>,
    ensure_visible: bool,
    is_selecting: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: "".into(),
            multiline: false,
            dark: false,
            fill_height: false,
            read_only: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_lines: Vec::new(),
            last_bounds: None,
            line_height: px(22.),
            scroll: point(px(0.), px(0.)),
            ensure_visible: true,
            is_selecting: false,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn with_placeholder(mut self, placeholder: &str) -> Self {
        self.placeholder = placeholder.to_owned().into();
        self
    }

    pub fn with_multiline(mut self, multiline: bool) -> Self {
        self.multiline = multiline;
        self
    }

    pub fn with_dark(mut self, dark: bool) -> Self {
        self.dark = dark;
        self
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        cx.notify();
    }

    pub fn with_fill_height(mut self, fill_height: bool) -> Self {
        self.fill_height = fill_height;
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        if read_only {
            self.selected_range = 0..0;
            self.scroll = point(px(0.), px(0.));
        }
        self
    }

    pub fn text(&self) -> String {
        self.content.to_string()
    }
    pub fn set_placeholder(&mut self, text: &str, cx: &mut Context<Self>) {
        self.placeholder = text.to_owned().into();
        cx.notify();
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = normalize_text(text, self.multiline);
        if self.content.as_ref() == text {
            return;
        }
        self.push_undo();
        self.content = text.into();
        let end = if self.read_only {
            0
        } else {
            self.content.len()
        };
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.ensure_visible = true;
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            content: self.content.clone(),
            selection: self.selected_range.clone(),
            reversed: self.selection_reversed,
        }
    }

    fn push_undo(&mut self) {
        if self.undo.last().is_none_or(|s| s.content != self.content) {
            self.undo.push(self.snapshot());
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    fn restore(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        self.content = snapshot.content;
        self.selected_range = snapshot.selection;
        self.selection_reversed = snapshot.reversed;
        self.marked_range = None;
        self.changed(cx);
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if let Some(previous) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(previous, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if let Some(next) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(next, cx);
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked_range.is_none() {
            cx.emit(InputEvent::Submit);
        }
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor_offset())
        } else {
            self.selected_range.start
        };
        self.move_to(offset, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.selected_range.is_empty() {
            self.next_boundary(self.cursor_offset())
        } else {
            self.selected_range.end
        };
        self.move_to(offset, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn vertical_offset(&self, down: bool) -> usize {
        let cursor = self.cursor_offset();
        let current = self.line_index(cursor);
        let target = if down {
            (current + 1).min(self.last_lines.len().saturating_sub(1))
        } else {
            current.saturating_sub(1)
        };
        match (self.last_lines.get(current), self.last_lines.get(target)) {
            (Some(current), Some(target)) => {
                let x = current
                    .layout
                    .x_for_index(cursor.saturating_sub(current.start));
                target.start + target.layout.closest_index_for_x(x)
            }
            _ => cursor,
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.vertical_offset(false), cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.vertical_offset(true), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.vertical_offset(false), cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.vertical_offset(true), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn line_start(&self) -> usize {
        self.content[..self.cursor_offset()]
            .rfind('\n')
            .map_or(0, |ix| ix + 1)
    }

    fn line_end(&self) -> usize {
        let cursor = self.cursor_offset();
        self.content[cursor..]
            .find('\n')
            .map_or(self.content.len(), |ix| cursor + ix)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_start(), cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_end(), cx);
    }

    fn document_home(&mut self, _: &DocumentHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_start(), cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_end(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.click_count >= 3 {
            self.move_to(0, cx);
            self.select_to(self.content.len(), cx);
        } else if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.last_bounds else {
            return;
        };
        let delta = event.delta.pixel_delta(self.line_height);
        let width = self.last_lines.iter().fold(px(0.), |width, line| {
            if line.layout.width > width {
                line.layout.width
            } else {
                width
            }
        });
        let max_x = (width - bounds.size.width + px(3.)).max(px(0.));
        let max_y =
            (self.line_height * self.last_lines.len() as f32 - bounds.size.height).max(px(0.));
        self.scroll.x = (self.scroll.x - delta.x).max(px(0.)).min(max_x);
        self.scroll.y = (self.scroll.y - delta.y).max(px(0.)).min(max_y);
        self.ensure_visible = false;
        cx.stop_propagation();
        cx.notify();
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if !self.selected_range.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.ensure_visible = true;
        let offset = char_boundary(&self.content, offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn line_index(&self, offset: usize) -> usize {
        self.last_lines
            .iter()
            .rposition(|line| line.start <= offset)
            .unwrap_or(0)
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        if self.content.is_empty() || self.last_lines.is_empty() {
            return 0;
        }
        let y = position.y - bounds.top() + self.scroll.y;
        let row = ((y / self.line_height).floor().max(0.) as usize).min(self.last_lines.len() - 1);
        let line = &self.last_lines[row];
        char_boundary(
            &self.content,
            line.start
                + line
                    .layout
                    .closest_index_for_x(position.x - bounds.left() + self.scroll.x),
        )
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.ensure_visible = true;
        let offset = char_boundary(&self.content, offset);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        utf8_to_utf16(&self.content, range.start)..utf8_to_utf16(&self.content, range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let start = utf16_to_utf8(&self.content, range.start);
        start..utf16_to_utf8(&self.content, range.end).max(start)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < offset).then_some(ix))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > offset).then_some(ix))
            .unwrap_or(self.content.len())
    }
}

fn normalize_text(text: &str, multiline: bool) -> String {
    let text = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\0', "");
    if multiline {
        text
    } else {
        text.replace('\n', " ")
    }
}

fn char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn utf16_to_utf8(text: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    for (utf8, ch) in text.char_indices() {
        if utf16 + ch.len_utf16() > offset {
            return utf8;
        }
        utf16 += ch.len_utf16();
    }
    text.len()
}

fn utf8_to_utf16(text: &str, offset: usize) -> usize {
    text[..char_boundary(text, offset)].encode_utf16().count()
}

/// IME selection is expressed relative to the inserted text, not the document.
fn marked_selection(start: usize, inserted: &str, relative: Option<Range<usize>>) -> Range<usize> {
    relative.map_or_else(
        || start + inserted.len()..start + inserted.len(),
        |range| {
            let lower = utf16_to_utf8(inserted, range.start);
            let upper = utf16_to_utf8(inserted, range.end).max(lower);
            start + lower..start + upper
        },
    )
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let new_text = normalize_text(new_text, self.multiline);
        if self.marked_range.is_none() {
            self.push_undo();
        }
        self.content =
            (self.content[..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        let end = range.start + new_text.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.changed(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let new_text = normalize_text(new_text, self.multiline);
        if self.marked_range.is_none() {
            self.push_undo();
        }
        self.content =
            (self.content[..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        self.marked_range =
            (!new_text.is_empty()).then_some(range.start..range.start + new_text.len());
        self.selected_range = marked_selection(range.start, &new_text, new_selected_range_utf16);
        self.selection_reversed = false;
        self.changed(cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let row = self.line_index(range.start);
        let line = self.last_lines.get(row)?;
        let y = bounds.top() + self.line_height * row as f32 - self.scroll.y;
        Some(Bounds::from_corners(
            point(
                bounds.left()
                    + line
                        .layout
                        .x_for_index(range.start.saturating_sub(line.start))
                    - self.scroll.x,
                y,
            ),
            point(
                bounds.left()
                    + line.layout.x_for_index(
                        range
                            .end
                            .min(line.start + line.layout.len())
                            .saturating_sub(line.start),
                    )
                    - self.scroll.x,
                y + self.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.last_bounds?;
        Some(utf8_to_utf16(
            &self.content,
            self.index_for_mouse_position(position),
        ))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    lines: Vec<DisplayLine>,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
    scroll: Point<Pixels>,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let input = self.input.read(cx);
        let visible_lines = if input.multiline {
            input.content.split('\n').count().clamp(3, 7)
        } else {
            1
        };
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = if input.fill_height {
            relative(1.).into()
        } else {
            (window.line_height() * visible_lines as f32).into()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let input = self.input.read(cx);
        let colors = Colors::new(input.dark);
        let style = window.text_style();
        let line_height = window.line_height();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let display = if input.content.is_empty() {
            &input.placeholder
        } else {
            &input.content
        };
        let color = if input.content.is_empty() {
            rgb(colors.placeholder).into()
        } else {
            rgb(colors.text).into()
        };
        let mut start = 0;
        let mut lines = Vec::new();
        for text in display.split('\n') {
            let end = start + text.len();
            let run = TextRun {
                len: text.len(),
                font: style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let mut runs = Vec::new();
            if let Some(marked) = input
                .marked_range
                .as_ref()
                .filter(|r| r.start < end && r.end > start)
            {
                let lower = marked.start.max(start) - start;
                let upper = marked.end.min(end) - start;
                if lower > 0 {
                    runs.push(TextRun {
                        len: lower,
                        ..run.clone()
                    });
                }
                runs.push(TextRun {
                    len: upper - lower,
                    underline: Some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                });
                if upper < text.len() {
                    runs.push(TextRun {
                        len: text.len() - upper,
                        ..run
                    });
                }
            } else {
                runs.push(run);
            }
            lines.push(DisplayLine {
                start,
                layout: window.text_system().shape_line(
                    text.to_owned().into(),
                    font_size,
                    &runs,
                    None,
                ),
            });
            start = end + 1;
        }
        let cursor_offset = input.cursor_offset();
        let cursor_row = lines
            .iter()
            .rposition(|line| line.start <= cursor_offset)
            .unwrap_or(0);
        let cursor_x = lines[cursor_row]
            .layout
            .x_for_index(cursor_offset.saturating_sub(lines[cursor_row].start));
        let cursor_y = line_height * cursor_row as f32;
        let mut scroll = input.scroll;
        if input.ensure_visible {
            if cursor_x < scroll.x {
                scroll.x = cursor_x;
            }
            if cursor_x > scroll.x + bounds.size.width - px(3.) {
                scroll.x = cursor_x - bounds.size.width + px(3.);
            }
            if cursor_y < scroll.y {
                scroll.y = cursor_y;
            }
            if cursor_y + line_height > scroll.y + bounds.size.height {
                scroll.y = cursor_y + line_height - bounds.size.height;
            }
        }
        let width = lines
            .iter()
            .fold(px(0.), |width, line| width.max(line.layout.width));
        scroll.x = scroll
            .x
            .max(px(0.))
            .min((width - bounds.size.width + px(3.)).max(px(0.)));
        scroll.y = scroll
            .y
            .max(px(0.))
            .min((line_height * lines.len() as f32 - bounds.size.height).max(px(0.)));
        let mut selections = Vec::new();
        if !input.selected_range.is_empty() {
            for (row, line) in lines.iter().enumerate() {
                let end = line.start + line.layout.len();
                let selection = &input.selected_range;
                if selection.start <= end && selection.end > line.start {
                    let left = line
                        .layout
                        .x_for_index(selection.start.saturating_sub(line.start));
                    let mut right = line
                        .layout
                        .x_for_index(selection.end.min(end).saturating_sub(line.start));
                    if selection.end > end {
                        right += px(5.);
                    }
                    selections.push(fill(
                        Bounds::new(
                            point(
                                bounds.left() + left - scroll.x,
                                bounds.top() + line_height * row as f32 - scroll.y,
                            ),
                            size(right - left, line_height),
                        ),
                        rgba(colors.selection_rgba),
                    ));
                }
            }
        }
        let cursor = input.selected_range.is_empty().then(|| {
            fill(
                Bounds::new(
                    point(
                        bounds.left() + cursor_x - scroll.x,
                        bounds.top() + cursor_y - scroll.y,
                    ),
                    size(px(1.5), line_height),
                ),
                rgb(colors.accent),
            )
        });
        PrepaintState {
            lines,
            cursor,
            selections,
            scroll,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus_handle.clone();
        let line_height = window.line_height();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for selection in prepaint.selections.drain(..) {
                window.paint_quad(selection);
            }
            for (row, line) in prepaint.lines.iter().enumerate() {
                let origin = point(
                    bounds.left() - prepaint.scroll.x,
                    bounds.top() + line_height * row as f32 - prepaint.scroll.y,
                );
                let _ = line.layout.paint(origin, line_height, window, cx);
            }
            if focus.is_focused(window) && !self.input.read(cx).read_only {
                if let Some(cursor) = prepaint.cursor.take() {
                    window.paint_quad(cursor);
                }
            }
        });
        self.input.update(cx, |input, _| {
            input.last_lines = std::mem::take(&mut prepaint.lines);
            input.last_bounds = Some(bounds);
            input.line_height = line_height;
            input.scroll = prepaint.scroll;
            input.ensure_visible = false;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let colors = Colors::new(self.dark);
        crate::controls::field(colors, focused)
            .w_full()
            .min_w_0()
            .when(!self.multiline && !self.fill_height, |element| {
                element.h(px(crate::controls::HEIGHT))
            })
            .when(self.fill_height, |element| {
                element.h_full().flex().flex_col()
            })
            .key_context("TextInput")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .line_height(px(22.))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::document_home))
            .on_action(cx.listener(Self::document_end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::newline))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .child(
                div()
                    .w_full()
                    .when(self.fill_height, |element| {
                        element.flex_1().min_h_0().h_full()
                    })
                    .px_2()
                    .when(!self.multiline, |element| element.py(px(2.)))
                    .when(self.multiline, |element| element.py_1())
                    .child(TextElement { input: cx.entity() }),
            )
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_roundtrip_handles_emoji_and_combining_text() {
        let text = "e\u{301}🙂漢";
        for (utf8, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), '\0')))
        {
            assert_eq!(utf16_to_utf8(text, utf8_to_utf16(text, utf8)), utf8);
        }
        assert_eq!(utf16_to_utf8(text, 3), 3); // middle of emoji surrogate pair
        assert_eq!(utf16_to_utf8(text, 999), text.len());
    }

    #[test]
    fn ime_selection_uses_inserted_text_and_start_offset() {
        assert_eq!(marked_selection(10, "é🙂", Some(1..3)), 12..16);
        assert_eq!(marked_selection(10, "漢", Some(1..1)), 13..13);
        assert_eq!(marked_selection(10, "", None), 10..10);
        assert_eq!(marked_selection(10, "漢", Some(0..100)), 10..13);
    }

    #[test]
    fn pasted_line_endings_follow_input_mode() {
        assert_eq!(normalize_text("a\r\nb\rc\0", true), "a\nb\nc");
        assert_eq!(normalize_text("a\r\nb\rc\0", false), "a b c");
    }
}
