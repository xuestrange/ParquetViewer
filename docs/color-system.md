# Data table color system

Parquet Viewer uses neutral gray layers to organize dense data and indigo to distinguish focus, selection and primary actions. IBM Carbon informed the neutral layering. Radix Colors' Slate and Iris scales supplied starting values, adapted for the table, three mark colors and compact controls. This application owns its theme, token assignments and contrast checks. Light and dark appearances share the same semantic roles. See [theme.rs](../src/theme.rs) and [MarkColor](../src/table_state.rs).

## Design sources

The following official sources were verified on **2026-10-31**.

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

The adapted values reduce differences in surface, grid, stripe and selection lightness. Their combinations require independent contrast checks. Dark `input_border=#777B84` uses the exact Slate dark 10 color in a role assigned by the application. Input boundaries have more visual weight than decorative grid lines. The background change retained the existing foregrounds for NULL, marks and text, and the established input outlines.

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

The compact design combines commands with the 36 px custom title bar, retains the 22 px status bar, shows filters on demand and groups more complex actions in secondary menus. The color change adds no persistent toolbar. Typed Arrow results and virtualization on both axes retain table density, frozen panes, NULL semantics and a selection outline painted outside layout.

Single and rectangular selections have a **2 pt indigo outline**. The table paints one overlay, aligning all four edges to device pixels. A whole-column selection encloses its header and data together. The outline continues across frozen boundaries and clips at the viewport. Grid lines inside the selection take the highlighted cell fill. Marks keep their own hues.

A selected 17 px rounded checkbox uses `primary` with a white checkmark. Inputs use `input_border` at rest and `accent` for focus. Primary buttons use white foreground in both themes, matching icon-cache conventions. Disabled controls use secondary foregrounds and keep labels legible without repeatedly reducing opacity.

The application, CSV and Parquet files use distinct icons. See the [artwork and packaging notes](../packaging/assets/README.md).

## Stable title-bar colors for independent windows

Each main data window receives a monotonically increasing color index when created. Closing another window does not renumber survivors. Tables, inputs, results and previews use the shared theme. Only the 36 px title bar overrides its background via `bar_with_tint`. Normal and secondary title text and SVG foregrounds retain semantic colors.

The first eight presets prioritize distinct hues in typical small window sets.

| Index | Hue | Light title bar | Dark title bar |
|---|---|---|---|
| 0 | Blue | `#EFF6FE` | `#1C232B` |
| 1 | Teal | `#EFFBF9` | `#1C2826` |
| 2 | Violet | `#F9F2FE` | `#261F2B` |
| 3 | Rose | `#FEF2F7` | `#2B1F24` |
| 4 | Amber | `#FEF9EF` | `#2B261C` |
| 5 | Green | `#F3FBF1` | `#20281E` |
| 6 | Ice cyan | `#EFFCFE` | `#1C292B` |
| 7 | Graphite | `#F4F6F8` | `#212325` |

Each RGB channel in the color key ranges from 0 to 15. The light mapping is `239 + value` and the dark mapping is `28 + value`. The ninth window continues the low-saturation sequence, which repeats only after 4096 distinct numeric colors. Human observers cannot necessarily distinguish all of them. Keys are generated at compile time. Rendering performs a constant-time lookup with no per-frame allocation or contrast calculation. Existing tests cover the 4.5:1 threshold for primary, secondary and interactive foregrounds over all 4096 backgrounds in both themes, stable indices and the repeat boundary. Earlier GUI checks for blue/teal/violet windows and inherited preview colors belong to their earlier palette. See [theme.rs](../src/theme.rs) and [window_chrome.rs](../src/window_chrome.rs).

## Theme modes and system appearance

The theme implementation documented here offered Light, Dark and Follow System in **More** and the native **Edit → Theme** submenu. New installations default to Follow System. Older preferences with only `dark: true/false` migrate to explicit Dark/Light to retain the user's choice. The preference stores the mode separately from the effective appearance so system changes cannot override fixed modes.

Follow System resolves GPUI appearance events and updates all main windows, existing TextInputs, filter editors, import wizards, owned Quick Peek views and their text inputs. Windows share persistent preferences and merge individual changes on save to avoid overwriting another window's newer settings. Query state remains per-window. AppKit matching maps high-contrast system appearances to the corresponding light or dark base palette. This neither changes the system appearance nor supplies a separate high-contrast palette. See [theme.rs](../src/theme.rs) and [app.rs](../src/app.rs).

## Status-bar memory metric

The 22 px status bar displays the current process's **physical footprint** from the macOS public libproc API `proc_pid_rusage(RUSAGE_INFO_V0)`, field `ri_phys_footprint`. Footprint can include compressed memory. RSS, historical peak, the former DuckDB budget and total system memory use different definitions. The field rounds in IEC units and displays `—` if unavailable.

A single application heartbeat samples in the background roughly every two seconds and broadcasts results to all data windows. A process-wide cache also coalesces near-simultaneous requests. Adding windows adds no sampling loops, and the displayed per-process numbers cannot be summed across windows. Rendering formats cached values. Non-macOS platforms report unavailable. Sampling time and accounting definitions can differ among tools, including Activity Monitor. See [process_memory.rs](../src/process_memory.rs) for implementation and boundary tests.

## Historical columnar rendering and resource policy

In the Arrow-storage, pre-DataFusion build documented below, the table rendered visible cells with 120 px of overscan on each edge. Frozen panes had separate range calculations. Arrow validity supplied NULL foregrounds directly. Only wrapped columns contributed to visible row-height calculations, so horizontal scrolling retained the row heights. Ordinary display strings were limited to 2000 Unicode characters while copying, saving and exporting used full values. Each window had a `SharedString` LRU budget of 8 MiB when active and 1 MiB when inactive. Coloring required no full scan or separate NULL cache.

That build's `ArrowTable` retained typed `RecordBatch` values and the entire requested range, with a separate 128 MiB Arrow array budget per result. UNION leaves used exact JSON UTF-8 for NULL/branch tags the native conversion would lose. Complex MAP keys carried an auxiliary exact spelling. Other fields kept their types. Image previews avoided JSON. Structured cell JSON/model preparation ran off the UI thread. Explicit image Copy Value serialized one shallow cell slice, which could retain a buffer in its column and batch without pinning unrelated columns or batches.

At that revision, each window's DuckDB used all available CPUs without subtracting a worker, imposing a 16-core ceiling, gating global concurrency at two or sharing an engine resource pool. Managed memory per engine was workers × 128 MiB, clamped to 512–2048 MiB. Result, cache and engine budgets were separate and did not cap process RSS. The status bar continued to sample process footprint with one heartbeat. Subsequent architecture and compatibility records are in [data behavior and historical validation](data-behavior.md).

## Historical validation of Arrow storage and the greedy policy

Final build `982A499C-78E0-37F0-AB48-D67553F55204` reported **160 passed, 0 failed, 1 ignored** (audio hardware; 161 total). Formatting, strict Clippy, optimized packaging and strict signature verification passed for release/dist/QA.

The complete wide-table, NULL, frozen-pane, theme and multiwindow GUI checks belong to baseline `119E7A1E-3768-390C-BDEB-76F7E07EE523`. It reported 159 passed, 0 failed and 1 ignored (160 total), with formatting, strict Clippy, release and strict signing checks passing. The follow-up changed image Copy Value in `app.rs` and `peek.rs`. Its narrower final-package smoke check is reported separately.

- In blue window 1, the baseline opened a thousand-field wide table, first 1000 rows and then all 5000, checked first/last navigation and the last row/column value, froze the first two rows and column 0, and opened automatic column width. Teal window 2 opened the 100000-row NULL fixture. Asynchronous items/records details displayed Chinese text, 😀, a NULL record and a null field. Save Value read back independently as `[0,null,1]`, and Return navigated back. After filtering window 2 to `id=1` and switching to dark, it verified NULL foreground-only styling, all four selection edges and normal styling of literal `NULL`. Window 1 retained all 5000 rows, frozen panes and its final-cell selection. Both windows shared the new theme and retained their blue and teal titles.
- Headless debug checks measured 5,823,733 bytes of Arrow array storage for the 100000×8 NULL fixture, 9,385,464 bytes for a 1000×1000 wide table and 46,975,871 bytes for 5000×1000. Corresponding queries took 266/671/1253 ms. These were single-run array storage/correctness checks. Release GUI Load All labels were 248 ms (wide) and 40 ms (NULL). The headless report also checked concurrent Engines, old snapshots, image payloads without JSON, and source hash/mtime. These observations cannot establish process RSS, GUI FPS or controlled speedups.
- A manual horizontal scrollbar drag in the baseline's dark wide table brought columns 494–504 into view, with col500 aligned and the first column/two rows frozen without blanks. A violet window previewed genuine 16,516-byte PNGs from both Photo STRUCT and BLOB. Copy Value completed in the background and showed Copied. Clipboard contents were not read. Return worked, light mode was restored, and closing the photo and wide windows left the NULL window filtered to `id=1` with the selected NULL intact. The QA instance then exited, without restarting the original process, running caffeinate or changing the system appearance.

The final fix in `982A499C-78E0-37F0-AB48-D67553F55204` captured the BLOB preview's `BinaryFormat` when opened. Explicit Copy Value borrows all bytes in the background and applies that format, replacing the former `<binary: n bytes>` summary. Photo STRUCT still produces full JSON containing its path and bytes. Added regressions covered thirteen formats and three input lengths, including 4096-byte values beyond the preview cutoff, plus Photo JSON paths and bytes.

In the final bundle, `photo-ui.parquet` loaded all seven fields. Double-clicking either `binary_image` BLOB or Photo STRUCT showed a real 16,516-byte PNG. Both Copy Value operations displayed “Copied to clipboard” and Return selected the original cell. The GUI verified the preview, completion and navigation; the automated checks covered copied content. Command-Q exited the standalone QA instance without restarting the user's app, running caffeinate or changing system settings.

Independent debug regressions on the same backend also passed for 5 GB row ranges. Parquet's first/last 1000 rows took 69/40 ms with wait4 maximum RSS of 168.23 MiB. Text took 111/51698 ms and 1023.69 MiB. Distant text offsets still required scanning. These were range checks, separate from GUI or full-file Arrow loads. RSS included native and hash subprocess scope, outside the 128 MiB Arrow result budget, and total wall time included file hashing. Failed time-l/sysctl wrapper logs were retained. Both direct wait4 measurements exited 0, and original/hard-linked sources retained SHA, mtime, size and inode. The final image-copy patch did not rerun this backend. FPS, p95, CPU profiles, controlled speed/RSS improvements and the full media suite were outside scope. See [feature checks](data-behavior.md) and [arrow_columnar_greedy](large-file-validation.json).

## Earlier NULL text and independent-window build

This version introduced foreground-only NULLs, stable corresponding title-bar tints for main windows and Peek, shared theme/preferences, and a single heartbeat for Memory. Its old scheduler allowed at most two simultaneous heavy open/import/query/export jobs. Queued jobs could be cancelled and RAII returned permits on completion or error. The subsequent greedy policy superseded that limit.

`cargo test --locked` reported **139 passed, 0 failed, 1 ignored** (audio hardware; 140 total). Formatting, strict Clippy and optimized packaging passed. Dist and the independent QA copy passed strict deep signature verification under UUID `26D95E7F-2160-3FDE-ADF8-78A79A19B01C`.

The blue main window opened 100000 NULL-fixture rows and reached id `99999`. True NULL in light/dark tables and selected/unselected nested nulls displayed rose text over their previous backgrounds. Independently filtering the teal demo to one row preserved all rows and selection in blue. Switching blue to dark synchronized teal without merging their title colors. Blue Peek inherited its parent tint. A violet window opened from Peek inherited dark mode. Closing violet returned to Peek, and Return restored the parent table. Closing blue preserved teal's filtered result; light mode and the teal title were restored. Command-backtick and the native Window menu both confirmed that only teal window 2 remained. The blue window, its preview and the violet window had all closed.

FPS, p95 and CPU profiles, the 5 GB run, manual import preview, external Finder routing and the full media suite were outside this pass. Finder routing had code/automated coverage. Numeric color regressions do not test human distinguishability of every tint or whole-application accessibility. See [null_text_only_multiwindow](large-file-validation.json).

## Earlier NULL-background experiment

Build `A3F2F481-B690-3A6D-8CD5-07421A9655C2` reported **128 passed, 0 failed, 1 ignored** (129 total), with formatting, strict Clippy, release and strict verification passing. Checks covered the high-NULL fixture at 1000 rows and Load All, scrolling to the end, foregrounds over dark selection and the three marks, nested nulls/NULL records and Return. This build used a subtle NULL background later replaced by the foreground-only treatment. It did not repeat import-preview GUI checks, FPS/p95/CPU profiling, the 5 GB workload or the original main process launch.

## Earlier system theme, selection and memory build

The suite reported **124 passed, 0 failed, 1 ignored** (audio hardware; 125 total). Formatting, strict Clippy, optimized build and strict package signature checks passed. Release, dist and standalone QA had the same executable UUID, `5B058764-D3B9-330C-A643-EE9F32518F7C`.

Using the native picker, the standalone release opened a demo with 1000 rows and eleven fields. It verified all four edges of single-cell selections in light/dark, dark drag selection, light row/column-header selection and a closed outline across four frozen panes. The status bar retained its 22 px height and Memory updated from welcome to table. More offered three mutually exclusive theme items and Follow System had the expected checkmark. Selecting it matched the system's current light appearance. The native Edit → Theme command changed an already-open nested Peek to dark and back to light. The table and JSON input updated together, and Return to Cell succeeded. The original fixed light setting was restored without restarting the user's existing process. The run left macOS global appearance alone, so automatic system transitions and high-contrast changes had code/regression coverage only. It did not repeat 5 GB or full media tests. See [system_theme_selection_memory](large-file-validation.json).

## Earlier neutral-palette build

Official references and color mappings were checked. The suite reported **112 passed, 0 failed, 1 ignored** (audio hardware; 113 total). Formatting, strict Clippy, optimized build and strict signing passed for release/dist UUID `4F0DEF03-9B31-36BC-B9A4-E1C4FA05C83A`. Once the Mac was unlocked, the packaged application was used for the following checks.

- Dark welcome; a 1000-row, eleven-field demo with headers, stripes, text, true NULL and nested links.
- In both themes, default/focused field-search outlines, select all/none, readable disabled Apply 0, normal primary buttons and modal overlays. Primary-button hover was checked in dark mode only.
- In both themes, default/focused sidebar inputs, selected All and the Not applied message. A dark-mode `id=0` filter produced one result. Clearing the draft and applying restored all 1000.
- All three row marks in light/dark, retained hue for a selected Amber row, Blue column fill and header stripe, and ordinary column selection. The run ended in dark mode.

The import wizard's six inputs later received theme synchronization under release/dist UUID `F21D8AFF-A939-3A9D-BB8C-6C88661D6746`. The same 112/0/1 suite, formatting, strict Clippy, optimized packaging and strict signing passed. The final bundle relaunched and showed all six inputs in the dark import panel. In-place switching in that panel did not receive a separate GUI check. The more extensive palette checks above belong to `4F0DEF03-9B31-36BC-B9A4-E1C4FA05C83A`. Large-file performance, the complete media suite and the old standalone preview were untouched in this pass. Temporary caffeinate stopped, persistent power settings were preserved and the final main window retained the user's state. These contrast values and scoped GUI checks do not establish whole-application accessibility.
