//! Compact, shared window chrome while retaining AppKit's native window behavior.
//!
//! Use `Some(options(title))` when opening a window with this toolbar.
//! AppKit handles the traffic lights and `drag_region` starts window drags.

// objc 0.2's selector macros check this legacy cfg in the consuming crate.
#![allow(unexpected_cfgs)]

use gpui::{
    Div, MouseButton, SharedString, Stateful, TitlebarOptions, Window, div, point, prelude::*, px,
    rgb,
};

use crate::theme::{self, Colors};

pub const HEIGHT: f32 = 36.;
const TRAFFIC_LIGHT_INSET: f32 = 72.;

/// Let the toolbar draw behind the native titlebar controls.
pub fn options(title: Option<SharedString>) -> TitlebarOptions {
    TitlebarOptions {
        title,
        appears_transparent: true,
        traffic_light_position: Some(point(px(14.), px(11.))),
    }
}

/// AppKit handles fullscreen controls in its own titlebar above the toolbar.
pub fn leading_inset(window: &Window) -> f32 {
    if cfg!(target_os = "macos") && !window.is_fullscreen() {
        TRAFFIC_LIGHT_INSET
    } else {
        0.
    }
}

pub fn bar(dark: bool, window: &Window) -> Stateful<Div> {
    let c = Colors::new(dark);
    div()
        .id("window-chrome")
        .h(px(HEIGHT))
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(2.))
        .px_2()
        .pl(px(8. + leading_inset(window)))
        .bg(rgb(c.bar))
        .text_color(rgb(if window.is_window_active() {
            c.text
        } else {
            c.muted
        }))
        .border_b_1()
        .border_color(rgb(c.border))
        .occlude()
}

/// Main windows keep a stable color ordinal; owned Peeks share the same one.
/// Ordinary foregrounds and controls remain the shared semantic theme colors.
pub fn bar_with_tint(dark: bool, window: &Window, ordinal: u64) -> Stateful<Div> {
    bar(dark, window).bg(rgb(theme::window_tint(ordinal, dark)))
}

pub fn drag_region() -> Stateful<Div> {
    div()
        .id("window-drag-region")
        .h_full()
        .flex_1()
        .on_mouse_down(MouseButton::Left, |event, window, cx| {
            cx.stop_propagation();
            if event.click_count == 2 {
                window.titlebar_double_click();
            } else {
                start_drag(window);
            }
        })
}

fn start_drag(window: &Window) {
    #[cfg(target_os = "macos")]
    if let Err(error) = macos::start_drag(window) {
        log::warn!("Could not start native window drag: {error:#}");
    }
    #[cfg(not(target_os = "macos"))]
    window.start_window_move();
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Result, anyhow, ensure};
    use gpui::Window;
    use objc::{
        msg_send,
        runtime::{BOOL, Class, Object, YES},
        sel, sel_impl,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}

    fn native_window(window: &Window) -> Result<*mut Object> {
        let thread_class =
            Class::get("NSThread").ok_or_else(|| anyhow!("macOS Foundation is unavailable."))?;
        // SAFETY: NSThread.isMainThread returns BOOL. No AppKit object is
        // accessed before confirming that this call is on its owning thread.
        let is_main: BOOL = unsafe { msg_send![thread_class, isMainThread] };
        ensure!(is_main == YES, "Window chrome must run on the main thread.");
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|error| anyhow!("The native window handle is unavailable: {error}"))?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return Err(anyhow!("The native window is not an AppKit window."));
        };
        let view = handle.ns_view.as_ptr().cast::<Object>();
        // SAFETY: GPUI's AppKit handle is its live NSView. Both the view and
        // the returned window are borrowed for this synchronous call only;
        // no retain, release or ownership transfer is performed.
        let native_window: *mut Object = unsafe { msg_send![view, window] };
        ensure!(!native_window.is_null(), "The native view has no window.");
        Ok(native_window)
    }

    pub(super) fn start_drag(window: &Window) -> Result<()> {
        let native_window = native_window(window)?;
        let application_class =
            Class::get("NSApplication").ok_or_else(|| anyhow!("macOS AppKit is unavailable."))?;
        // SAFETY: AppKit access stays on the checked main thread. The current
        // NSEvent is borrowed only within this mouseDown callback. The SDK
        // specifies that performWindowDragWithEvent: returns immediately and
        // may consume mouseUp; never defer the lookup or await that event.
        unsafe {
            let application: *mut Object = msg_send![application_class, sharedApplication];
            ensure!(
                !application.is_null(),
                "The macOS application is unavailable."
            );
            let event: *mut Object = msg_send![application, currentEvent];
            ensure!(!event.is_null(), "There is no current native mouse event.");
            let event_type: usize = msg_send![event, type];
            ensure!(
                event_type == 1,
                "A window drag requires a left mouse-down event."
            );
            let event_window: *mut Object = msg_send![event, window];
            ensure!(
                event_window == native_window,
                "The current mouse event belongs to another window."
            );
            let _: () = msg_send![native_window, performWindowDragWithEvent: event];
        }
        Ok(())
    }
}
