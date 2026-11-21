//! Shared semantic colors for the main window, inputs and inspectors.
//! Keep the data canvas quiet; reserve saturated color for actions and selection.

use gpui::WindowAppearance;
use serde::{Deserialize, Serialize};

/// User intent is separate from the effective light/dark rendering state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThemeMode {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemeMode {
    pub fn resolve(self, appearance: WindowAppearance) -> bool {
        match self {
            Self::Light => false,
            Self::Dark => true,
            Self::System => matches!(
                appearance,
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        }
    }
}

/// GPUI 0.2.2 recognizes only four exact AppKit appearance names. Ask AppKit
/// for its closest supported appearance so accessibility high-contrast variants
/// still resolve to the correct base light/dark mode. This reads system state;
/// it does not assign NSApplication.appearance or change macOS preferences.
pub(crate) fn system_appearance(fallback: WindowAppearance) -> WindowAppearance {
    #[cfg(target_os = "macos")]
    match macos::system_appearance() {
        Ok(appearance) => return appearance,
        Err(error) => log::warn!("Could not resolve the system appearance: {error:#}"),
    }
    fallback
}

const WINDOW_TINT_COUNT: usize = 4096;
const FIRST_WINDOW_TINT_KEYS: [u16; 8] = [
    0x07f, // blue
    0x0ca, // teal
    0xa3f, // violet
    0xf38, // rose
    0xfa0, // amber
    0x4c2, // green
    0x0df, // cyan
    0x579, // slate
];

/// The first eight hues are deliberate. An odd permutation of all 12-bit
/// color keys supplies the rest without repeating a color every eight windows.
/// This table is computed at compile time, not while rendering or opening a file.
const fn window_tint_keys() -> [u16; WINDOW_TINT_COUNT] {
    let mut keys = [0; WINDOW_TINT_COUNT];
    let mut next = 0;
    while next < FIRST_WINDOW_TINT_KEYS.len() {
        keys[next] = FIRST_WINDOW_TINT_KEYS[next];
        next += 1;
    }
    let mut step = 0;
    while step < WINDOW_TINT_COUNT {
        let key = ((step * 0x9e5 + 0x2cf) & 0xfff) as u16;
        let mut preset = 0;
        let mut is_preset = false;
        while preset < FIRST_WINDOW_TINT_KEYS.len() {
            if key == FIRST_WINDOW_TINT_KEYS[preset] {
                is_preset = true;
                break;
            }
            preset += 1;
        }
        if !is_preset {
            keys[next] = key;
            next += 1;
        }
        step += 1;
    }
    keys
}

const WINDOW_TINT_KEYS: [u16; WINDOW_TINT_COUNT] = window_tint_keys();

/// A stable titlebar identity for a main window and its Peeks.
/// The owner allocates the ordinal once and never renumbers surviving windows.
/// Light/dark variants retain the same RGB hue direction. There are 4096 unique
/// RGB backgrounds per theme before cycling; perceptual color separation is not
/// guaranteed for arbitrarily many simultaneous windows. No allocation or
/// background-dependent foreground calculation occurs on the rendering path.
pub(crate) fn window_tint(ordinal: u64, dark: bool) -> u32 {
    // Keep the extremes within the contrast range of the shared text colors.
    let (base, step) = if dark { (20, 3) } else { (225, 2) };
    let key = u32::from(WINDOW_TINT_KEYS[(ordinal % WINDOW_TINT_COUNT as u64) as usize]);
    let channel = |shift: u32| base + ((key >> shift) & 0xf) * step;
    (channel(8) << 16) | (channel(4) << 8) | channel(0)
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
mod macos {
    use anyhow::{Result, anyhow, ensure};
    use gpui::WindowAppearance;
    use objc::{
        msg_send,
        rc::autoreleasepool,
        runtime::{BOOL, Class, Object, YES},
        sel, sel_impl,
    };

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        static NSAppearanceNameAqua: *mut Object;
        static NSAppearanceNameDarkAqua: *mut Object;
    }

    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}

    pub(super) fn system_appearance() -> Result<WindowAppearance> {
        autoreleasepool(|| {
            let thread = Class::get("NSThread")
                .ok_or_else(|| anyhow!("macOS Foundation is unavailable."))?;
            // SAFETY: The class method returns BOOL. All subsequent AppKit
            // queries are synchronous and restricted to this main thread.
            let is_main: BOOL = unsafe { msg_send![thread, isMainThread] };
            ensure!(
                is_main == YES,
                "Appearance queries require the main thread."
            );
            let application = Class::get("NSApplication")
                .ok_or_else(|| anyhow!("macOS AppKit is unavailable."))?;
            let array =
                Class::get("NSArray").ok_or_else(|| anyhow!("macOS Foundation is unavailable."))?;
            // SAFETY: Signatures follow NSApplication.h / NSAppearance.h.
            // bestMatchFromAppearancesWithNames: is the public API for mapping
            // unsupported appearances, including high-contrast variants:
            // https://developer.apple.com/documentation/appkit/nsappearance/bestmatch(from:)
            // Objects are borrowed/autoreleased; no ownership is transferred.
            unsafe {
                let application: *mut Object = msg_send![application, sharedApplication];
                ensure!(
                    !application.is_null(),
                    "The macOS application is unavailable."
                );
                let appearance: *mut Object = msg_send![application, effectiveAppearance];
                ensure!(
                    !appearance.is_null(),
                    "The effective appearance is unavailable."
                );
                let names = [NSAppearanceNameAqua, NSAppearanceNameDarkAqua];
                let names: *mut Object =
                    msg_send![array, arrayWithObjects: names.as_ptr() count: names.len()];
                ensure!(!names.is_null(), "Could not create appearance names.");
                let matched: *mut Object =
                    msg_send![appearance, bestMatchFromAppearancesWithNames: names];
                ensure!(
                    !matched.is_null(),
                    "No supported appearance matches the system."
                );
                let is_dark: BOOL = msg_send![matched, isEqualToString: NSAppearanceNameDarkAqua];
                Ok(if is_dark == YES {
                    WindowAppearance::Dark
                } else {
                    WindowAppearance::Light
                })
            }
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Colors {
    pub bg: u32,
    pub surface: u32,
    pub bar: u32,
    pub header: u32,
    pub text: u32,
    pub muted: u32,
    pub frozen_text: u32,
    /// Real missing values use a purple-rose foreground, separate from errors.
    pub null_text: u32,
    pub border: u32,
    pub grid: u32,
    pub selected: u32,
    pub stripe: u32,
    pub hover: u32,
    pub accent: u32,
    pub accent_soft: u32,
    pub primary: u32,
    pub primary_hover: u32,
    pub on_primary: u32,
    pub input: u32,
    pub input_border: u32,
    pub placeholder: u32,
    pub selection_rgba: u32,
    pub scrollbar: u32,
    pub success: u32,
    pub success_bg: u32,
    pub blue_text: u32,
    pub memory_bg: u32,
    pub danger: u32,
    pub danger_bg: u32,
    pub warning: u32,
    pub warning_bg: u32,
    pub scrim: u32,
}

impl Colors {
    /// Choose a value foreground from semantic flags, never from its text.
    /// A literal string "NULL" and an empty string remain ordinary data.
    /// Callers retain the existing selection/mark background; no extra element
    /// or background-dependent contrast calculation is required per cell.
    pub fn value_foreground(self, is_null: bool, is_nested: bool) -> u32 {
        if is_null {
            self.null_text
        } else if is_nested {
            self.accent
        } else {
            self.text
        }
    }

    pub fn new(dark: bool) -> Self {
        // Neutral layering follows Radix Slate; Iris is reserved for focus,
        // selection and primary actions. A few interaction/text colors are
        // adapted for this dense table's contrast requirements. Sources and
        // exact mappings are recorded in docs/color-system.md.
        if dark {
            Self {
                bg: 0x141518,
                surface: 0x18191b,
                bar: 0x1b1c20,
                header: 0x1d1f23,
                text: 0xedeef0,
                muted: 0xb0b4ba,
                frozen_text: 0x79d9d0,
                null_text: 0xeeb4d7,
                border: 0x33363d,
                grid: 0x24262b,
                selected: 0x282c46,
                stripe: 0x1b1d21,
                hover: 0x25272d,
                accent: 0xb1a9ff,
                accent_soft: 0x26283d,
                primary: 0x5b5bd6,
                primary_hover: 0x6363d8,
                on_primary: 0xffffff,
                input: 0x1b1d21,
                input_border: 0x777b84,
                placeholder: 0xb0b4ba,
                selection_rgba: 0x8e8cff55,
                scrollbar: 0x696e77,
                success: 0x8bd7bd,
                success_bg: 0x153b32,
                blue_text: 0xa4ceff,
                memory_bg: 0x1c3452,
                danger: 0xffb1b2,
                danger_bg: 0x3b2026,
                warning: 0xf5ce8d,
                warning_bg: 0x3d311f,
                scrim: 0x00000088,
            }
        } else {
            Self {
                bg: 0xf7f8fa,
                surface: 0xffffff,
                bar: 0xf7f8fa,
                header: 0xf5f6f8,
                text: 0x1c2024,
                muted: 0x60646c,
                frozen_text: 0x00565b,
                null_text: 0x91366f,
                border: 0xd9dce3,
                grid: 0xeeeff2,
                selected: 0xedeefc,
                stripe: 0xfcfcfd,
                hover: 0xeeeff3,
                accent: 0x4747c2,
                accent_soft: 0xedeef9,
                primary: 0x5b5bd6,
                primary_hover: 0x5151cd,
                on_primary: 0xffffff,
                input: 0xffffff,
                input_border: 0x8b8d98,
                placeholder: 0x60646c,
                selection_rgba: 0x5b5bd633,
                scrollbar: 0x8b8d98,
                success: 0x18765c,
                success_bg: 0xe6f6eb,
                blue_text: 0x225c97,
                memory_bg: 0xe8f2fe,
                danger: 0xb42336,
                danger_bg: 0xfeebec,
                warning: 0x835a16,
                warning_bg: 0xfff5d6,
                scrim: 0x11111355,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Colors, ThemeMode, WINDOW_TINT_COUNT, window_tint};
    use crate::table_state::MarkColor;
    use gpui::WindowAppearance;

    #[test]
    fn theme_modes_follow_system_transitions_without_overriding_fixed_choices() {
        let appearances = [
            (WindowAppearance::Light, false),
            (WindowAppearance::Dark, true),
            (WindowAppearance::VibrantLight, false),
            (WindowAppearance::VibrantDark, true),
            (WindowAppearance::Light, false),
        ];
        for (appearance, expected_dark) in appearances {
            assert_eq!(ThemeMode::System.resolve(appearance), expected_dark);
            assert!(!ThemeMode::Light.resolve(appearance));
            assert!(ThemeMode::Dark.resolve(appearance));
        }
        assert_eq!(ThemeMode::default(), ThemeMode::System);
    }

    fn luminance(color: u32) -> f64 {
        let channel = |shift: u32| {
            let value = ((color >> shift) & 0xff_u32) as f64 / 255.;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    fn assert_contrast(foreground: u32, background: u32, minimum: f64, role: &str) {
        let a = luminance(foreground);
        let b = luminance(background);
        let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
        assert!(
            ratio >= minimum,
            "{role}: #{foreground:06x} on #{background:06x} is {ratio:.3}:1, expected {minimum}:1"
        );
    }

    #[test]
    fn table_and_panel_text_remains_readable_in_both_themes() {
        for dark in [false, true] {
            let c = Colors::new(dark);
            for background in [
                c.bg, c.surface, c.bar, c.header, c.stripe, c.hover, c.selected, c.input,
            ] {
                for foreground in [c.text, c.muted, c.accent] {
                    assert_contrast(foreground, background, 4.5, "table and panel text");
                }
            }
            assert_contrast(c.placeholder, c.input, 4.5, "input placeholder");
        }
    }

    #[test]
    fn real_null_foregrounds_remain_readable_on_table_and_mark_states() {
        for dark in [false, true] {
            let c = Colors::new(dark);
            // A NULL container must use the missing-value color rather than
            // looking like an expandable nested link. Test both cell kinds.
            for nested in [false, true] {
                let foreground = c.value_foreground(true, nested);
                assert_eq!(foreground, c.null_text);
                for background in [
                    c.bg, c.surface, c.bar, c.header, c.stripe, c.hover, c.selected, c.input,
                ] {
                    assert_contrast(foreground, background, 4.5, "real NULL text");
                }
                // Read the actual marking colors so future palette changes
                // cannot leave NULLs unreadable on a marked or selected row.
                for &mark in MarkColor::all() {
                    for background in [mark.background(dark), mark.selection_background(dark)] {
                        assert_contrast(foreground, background, 4.5, "marked real NULL text");
                    }
                }
            }
        }
    }

    #[test]
    fn frozen_text_remains_readable_on_table_and_mark_states() {
        for dark in [false, true] {
            let c = Colors::new(dark);
            for background in [c.surface, c.stripe, c.hover, c.header, c.bar, c.selected] {
                assert_contrast(c.frozen_text, background, 4.5, "frozen table text");
            }
            for &mark in MarkColor::all() {
                for background in [mark.background(dark), mark.selection_background(dark)] {
                    assert_contrast(c.frozen_text, background, 4.5, "marked frozen text");
                }
            }
        }
    }

    #[test]
    fn primary_hover_focus_and_input_boundary_have_sufficient_contrast() {
        for dark in [false, true] {
            let c = Colors::new(dark);
            for background in [c.primary, c.primary_hover] {
                assert_contrast(
                    c.on_primary,
                    background,
                    4.5,
                    "primary button text and checkmark",
                );
            }
            for background in [c.input, c.surface, c.stripe] {
                assert_contrast(
                    c.input_border,
                    background,
                    3.,
                    "input and unchecked box boundary",
                );
            }
            assert_contrast(c.accent, c.input, 3., "focus ring");
            assert_contrast(c.accent, c.accent_soft, 4.5, "accent command text");
        }
    }

    #[test]
    fn status_messages_keep_readable_foregrounds() {
        for dark in [false, true] {
            let c = Colors::new(dark);
            for (foreground, background) in [
                (c.success, c.success_bg),
                (c.blue_text, c.memory_bg),
                (c.warning, c.warning_bg),
                (c.danger, c.danger_bg),
            ] {
                assert_contrast(foreground, background, 4.5, "status message text");
            }
        }
    }

    #[test]
    fn window_tints_keep_all_titlebar_foregrounds_readable() {
        for dark in [false, true] {
            let c = Colors::new(dark);
            for ordinal in 0..WINDOW_TINT_COUNT as u64 {
                let background = window_tint(ordinal, dark);
                // Active/inactive titles, command labels, ordinary SVG glyphs,
                // and muted inactive window controls all use these semantics.
                for foreground in [c.text, c.muted, c.accent] {
                    assert_contrast(foreground, background, 4.5, "window titlebar foreground");
                }
            }
        }
    }

    #[test]
    fn window_tints_are_stable_and_do_not_repeat_after_eight_windows() {
        use std::collections::HashSet;

        for dark in [false, true] {
            let tints: HashSet<_> = (0..WINDOW_TINT_COUNT as u64)
                .map(|ordinal| window_tint(ordinal, dark))
                .collect();
            assert_eq!(tints.len(), WINDOW_TINT_COUNT);
            assert_ne!(window_tint(0, dark), window_tint(8, dark));
            assert_ne!(window_tint(1, dark), window_tint(9, dark));
            let ordinals = [0, 1, 7, 8, 39, 1023, 4095, u64::MAX];
            let before: Vec<_> = ordinals
                .iter()
                .map(|&ordinal| window_tint(ordinal, dark))
                .collect();
            // Opening/querying other windows cannot recolor existing ones.
            for ordinal in (0..128).rev() {
                let _ = window_tint(ordinal, !dark);
            }
            for (ordinal, expected) in ordinals.into_iter().zip(before) {
                assert_eq!(window_tint(ordinal, dark), expected);
                assert_eq!(
                    window_tint(ordinal, dark),
                    window_tint(ordinal % WINDOW_TINT_COUNT as u64, dark)
                );
            }
        }
        // A few explicit values protect the paired palette and rounding from
        // accidental changes while the uniqueness test covers the full space.
        assert_eq!(window_tint(0, false), 0xe1efff);
        assert_eq!(window_tint(0, true), 0x142941);
        assert_eq!(window_tint(3, false), 0xffe7f1);
        assert_eq!(window_tint(3, true), 0x411d2c);
    }
}
