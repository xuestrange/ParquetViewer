# Data table color system

Parquet Viewer uses neutral gray layers to organize dense data and indigo to distinguish focus, selection and primary actions. IBM Carbon informed the neutral layering. Radix Colors' Slate and Iris scales supplied starting values, adapted for the table, three mark colors and compact controls. This application owns its theme, token assignments and contrast checks. Light and dark appearances share the same semantic roles. See [theme.rs](../src/theme.rs) and [MarkColor](../src/table_state.rs).

## Design sources

Carbon separates regions with differences in neutral lightness, alternates light layers and makes higher dark layers lighter. It assigns separate tokens to backgrounds, text, borders and states. Parquet Viewer adopted that organization without importing Carbon components or copying an official theme. [Carbon color overview](https://www.carbondesignsystem.com/building-blocks/foundations/color/overview) and [color guidelines](https://www.carbondesignsystem.com/building-blocks/foundations/color/guidelines)

Radix describes twelve-step scales for backgrounds, interaction, borders, solid controls and text. Its composition guide pairs the cool Slate neutrals with Iris indigo. The application selects and adapts specific values and tests the resulting combinations itself. [Understanding the scale](https://www.radix-ui.com/colors/docs/palette-composition/understanding-the-scale) and [composing a palette](https://www.radix-ui.com/colors/docs/palette-composition/composing-a-palette)

The reference is pinned to [radix-ui/colors commit dbdb85470547c7d34b9001f48fddb08ded335979](https://github.com/radix-ui/colors/tree/dbdb85470547c7d34b9001f48fddb08ded335979). Its [package.json](https://github.com/radix-ui/colors/blob/dbdb85470547c7d34b9001f48fddb08ded335979/package.json) declares `@radix-ui/colors` version `3.0.0`.

- The original scales are in [light.ts](https://github.com/radix-ui/colors/blob/dbdb85470547c7d34b9001f48fddb08ded335979/src/light.ts) and [dark.ts](https://github.com/radix-ui/colors/blob/dbdb85470547c7d34b9001f48fddb08ded335979/src/dark.ts).
- The upstream [MIT license](https://github.com/radix-ui/colors/blob/dbdb85470547c7d34b9001f48fddb08ded335979/LICENSE) credits Modulz and WorkOS. The application distributes the attribution in [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).

The Rust theme contains the sRGB hexadecimal values it needs. Bundling the palette library, adding npm/JavaScript or Rust color dependencies, or downloading themes at runtime is unnecessary. The project does not claim to implement Radix's complete alpha or Display P3 scales.

## Neutral surfaces and interaction

Normal text, headers, input surfaces and grid lines use neutral colors. Indigo marks primary actions, focus, links and selection. Neutral table surfaces keep row and column marks legible. The table lists the main tokens. Local adaptations include both custom colors and application-specific uses of scale values.

| Token | Light | Dark | Source or use |
|---|---|---|---|
| `bg` | `#F7F8FA` | `#141518` | Window background adapted from Slate |
| `surface` | `#FFFFFF` | `#18191B` | Content and panels, white / Slate dark 2 |
| `bar` | `#F7F8FA` | `#1B1C20` | Default bar and status bar. Independent main windows override title-bar tint. |
| `header` | `#F5F6F8` | `#1D1F23` | Subtle local header layer |
| `stripe` | `#FCFCFD` | `#1B1D21` | Light Slate 1; locally reduced dark stripe contrast |
| `text` | `#1C2024` | `#EDEEF0` | Primary text, Slate 12 / Slate dark 12 |
| `frozen_text` | `#00565B` | `#79D9D0` | Frozen data, its row numbers and column names. True NULL uses `null_text` first. |
| `muted`, `placeholder` | `#60646C` | `#B0B4BA` | Secondary text, Slate 11 / Slate dark 11 |
| `border` | `#D9DCE3` | `#33363D` | Local region boundaries |
| `grid` | `#EEEFF2` | `#24262B` | Reduced visual weight for dense grids |
| `hover` | `#EEEFF3` | `#25272D` | Subtle local hover feedback |
| `input` | `#FFFFFF` | `#1B1D21` | White / locally adapted dark input background |
| `input_border` | `#8B8D98` | `#777B84` | Input outlines, Slate 9 / Slate dark 10 |
| `primary` | `#5B5BD6` | `#5B5BD6` | Primary buttons and selected checkboxes, Iris 9 / Iris dark 9 |
| `primary_hover` | `#5151CD` | `#6363D8` | Light Iris 10 / local dark adaptation |
| `on_primary` | `#FFFFFF` | `#FFFFFF` | Primary labels, icons and checkmarks |
| `accent` | `#4747C2` | `#B1A9FF` | Local light adaptation / Iris dark 11 |
| `accent_soft` | `#EDEEF9` | `#26283D` | Locally adapted subtle Iris background |
| `selected` | `#EDEEFC` | `#282C46` | Local selection fill for unmarked cells |

Light `accent=#4747C2` is used for links, input focus and interactive text on mark backgrounds. Upstream light Iris 11 is `#5753C6`. Dark `primary_hover=#6363D8` is also an adaptation, distinct from upstream Iris dark 10 (`#6E6ADE`).

The adapted values reduce differences in surface, grid, stripe and selection lightness. Their combinations require independent contrast checks. Dark `input_border=#777B84` uses the exact Slate dark 10 color in a role assigned by the application. Input boundaries have more visual weight than decorative grid lines.

Ordinary actions use a flat treatment with hover or selected fills. Primary actions, editable inputs and selected dates keep distinct states. Inputs and buttons use 3 px corner radii. Pop-up menus use 4 px corners and one fine border. Filter conditions use thin separators. The compact main header is 36 px and the status bar is 22 px.

Text selection uses `#5B5BD633` in light mode and `#8E8CFF55` in dark mode. Modal and busy overlays use `#11111355` and `#00000088` respectively for light and dark appearances. Their final contrast depends on the surface underneath.

## True NULL values

True NULL values use a separate rose foreground over the existing stripe, hover, selection or mark fill. The missing-data flag determines the color. Empty strings, the literal string `NULL` and numeric `NaN` retain their value semantics. If the import configuration explicitly treats a text token as missing, the parsed value determines the result. Missing values have a dedicated role independent of the error color `danger`.

| Token | Light | Dark | Use |
|---|---|---|---|
| `null_text` | `#91366F` | `#EEB4D7` | Rose foreground for true NULL over ordinary, selected and marked backgrounds |

`Colors::value_foreground(is_null, is_nested)` checks for an actual NULL before choosing a nested link or ordinary foreground. The same NULL foreground works over normal and striped rows, hover, selection and all three mark fills in their normal and selected states. Contrast regressions require at least 4.5:1 without computing luminance per cell.

The main table virtualizes both axes and creates elements only for the viewport and 120 px of overscan on each side. Frozen panes compute their own visible ranges. The foreground reads Arrow validity directly, avoiding a full scan, Cell or JSON materialization, per-cell badges and dedicated NULL caches. Highlighting leaves the loaded range and exported values intact.

Nested Quick Peek tables use the same foreground with ordinary or selected backgrounds. The model distinguishes JSON null, an absent field, an entire null record, an empty string and the literal `NULL`. The import wizard applies the same palette to parsed missing values in its five-row preview.

## Status and view marks

Success, error and warning use green, red and amber, with accompanying text. These application-specific combinations can include colors that also appear in upstream scales.

| Status | Light text / background | Dark text / background |
|---|---|---|
| Success | `#18765C` / `#E6F6EB` | `#8BD7BD` / `#153B32` |
| Error | `#B42336` / `#FEEBEC` | `#FFB1B2` / `#3B2026` |
| Warning | `#835A16` / `#FFF5D6` | `#F5CE8D` / `#3D311F` |

Blue, Teal and Amber row/column marks use visible fills. A selected marked cell keeps the mark hue and adds a selection outline. The row gutter and column-header underline also indicate the mark. Marks are view settings and leave data, query results and exports intact.

| Mark and theme | Normal fill | Selected fill | Stripe / menu swatch |
|---|---|---|---|
| Blue, light | `#E0E5FF` | `#D7E2FF` | `#5B5BD6` |
| Blue, dark | `#363F82` | `#333F88` | `#A899FF` |
| Teal, light | `#C4F1E5` | `#AFF3DF` | `#007A62` |
| Teal, dark | `#004E47` | `#004F45` | `#3CE6BD` |
| Amber, light | `#FFE3A5` | `#FFE1A0` | `#986300` |
| Amber, dark | `#603E00` | `#623E00` | `#FFCA55` |

Primary text, secondary text and nested links are checked against each normal and selected mark fill. Changing the surrounding Slate/Iris surfaces alone cannot guarantee their contrast.

## Layout and rendering

The compact design combines commands with the 36 px custom title bar, retains the 22 px status bar, shows filters on demand and groups more complex actions in secondary menus. Typed Arrow results and virtualization on both axes retain table density, frozen panes, NULL semantics and a selection outline painted outside layout.

Single and rectangular selections have a **2 pt indigo outline**. The table paints one overlay, aligning all four edges to device pixels. A whole-column selection encloses its header and data together. The outline continues across frozen boundaries and clips at the viewport. Grid lines inside the selection take the highlighted cell fill. Marks keep their own hues.

A selected 17 px rounded checkbox uses `primary` with a white checkmark. Inputs use `input_border` at rest and `accent` for focus. Primary buttons use white foreground in both themes, matching icon-cache conventions. Disabled controls use secondary foregrounds and keep labels legible without repeatedly reducing opacity.

The application, CSV and Parquet files use distinct icons. See the [artwork and packaging notes](../packaging/assets/README.md).

## Stable title-bar colors for independent windows

Each main data window receives a monotonically increasing color index when created. Closing another window does not renumber survivors. Tables, inputs, results and previews use the shared theme. Only the 36 px title bar overrides its background via `bar_with_tint`. Normal and secondary title text and SVG foregrounds retain semantic colors.

The first eight windows use blue, teal, violet, rose, amber, green, cyan and slate. Each theme has 4096 distinct title-bar colors before the sequence repeats. The contrast tests check primary text, secondary text and interactive foregrounds against each tint in both themes. See [theme.rs](../src/theme.rs) and [window_chrome.rs](../src/window_chrome.rs).

## Theme modes and system appearance

**Settings** offers Light, Dark and Follow System under **Theme**. New installations default to Follow System. Older preferences with only `dark: true/false` migrate to explicit Dark/Light to retain the user's choice. The preference stores the mode separately from the effective appearance so system changes cannot override fixed modes.

Follow System resolves GPUI appearance events and updates all main windows, existing TextInputs, filter editors, import wizards, owned Quick Peek views and their text inputs. Windows share persistent preferences and merge individual changes on save to avoid overwriting another window's newer settings. Query state remains per-window. AppKit matching maps high-contrast system appearances to the corresponding light or dark base palette. This neither changes the system appearance nor supplies a separate high-contrast palette. See [theme.rs](../src/theme.rs) and [app.rs](../src/app.rs).

## Status-bar memory metric

The 22 px status bar displays the current process's **physical footprint** from the macOS public libproc API `proc_pid_rusage(RUSAGE_INFO_V0)`, field `ri_phys_footprint`. Footprint can include compressed memory. The field rounds in IEC units and displays `—` if unavailable.

A single application heartbeat samples in the background roughly every two seconds and broadcasts results to all data windows. A process-wide cache also coalesces near-simultaneous requests. Adding windows adds no sampling loops, and the displayed per-process numbers cannot be summed across windows. Rendering formats cached values. Non-macOS platforms report unavailable. Sampling time and accounting definitions can differ among tools, including Activity Monitor. See [process_memory.rs](../src/process_memory.rs) for implementation and boundary tests.
