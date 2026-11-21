# Toolbar spacing and hierarchy (2026-11-03)

The 36 px toolbar kept all 16 commands on one row. Settings, file actions, field and analysis tools, and load controls occupied the left. The center displayed the filename and provided the window drag region. Metadata, export, help, and the window controls occupied the right.

At the standard width, eight commands had short captions and eight used persistent icon buttons. Tooltips included the full names and shortcuts. The shortened Chinese labels for aggregation, range, load all, and export were “汇总”, “范围”, “全部”, and “导出”. Buttons within a group were spaced 3 px apart and groups on the left 12 px apart. The design used fewer separators and no group backgrounds. After loading a file, Open used a neutral style. Active sidebars retained their indicators, and the filename had less visual emphasis.

GPUI measured the glyph widths. The layout reserved 174 px for fixed spacing and window controls. The filename could expand into remaining space, with a minimum of 96 px below a window width of 1140 px and 160 px otherwise. As space decreased, captions disappeared in priority order. Buttons remained available in place.

Production changes were confined to `src/toolbar.rs`, `src/i18n.rs`, and `src/i18n_zh.rs`. The Arrow/DataFusion engine, caches, queries, and export code were unchanged from the preceding revision. This pass collected no new comparative performance measurements.

The final release UUID was `AE6DCE06-934A-3B75-88E1-D6D7FFC640FA`. The suite recorded 213 passed, 0 failed, and 1 audio hardware test ignored. Formatting, strict all-target Clippy, release build, signing, and file-by-file ZIP round-trip checks passed. Regular files in the app totaled 101,864,048 logical bytes. The ZIP was 34,550,448 bytes.

A 1280 px debug window was checked with Chinese in dark mode and English in light mode, and with a 200,000-row, nine-field dataset. A separate debug build changed only the initial window width. At 1000 px, Chinese, English, and Turkish all displayed the 16 buttons and eight short captions. QA restored Simplified Chinese, system appearance, and the default date format. System appearance and power settings were untouched.

The bundle installed at `/Applications/Parquet Viewer.app` matched the files in `dist`. The previous copy was saved at `target/installed-app-backups/20261103-205709/Parquet Viewer.app`. Launching the installed release confirmed the preferences and toolbar layout with 1,000 loaded rows and nine fields. The UI reported 200,000 source rows. The isolated QA instance exited, and the release kept the data window open. After desktop tooling reported a user-initiated window state change, QA reconnected for observation. Full button-routing and media regression suites were outside that final check. [Validation record](toolbar-spacing-validation-2026-11-03.json)
