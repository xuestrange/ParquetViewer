# AGENTS.md

Parquet Viewer is a native macOS desktop app. GPUI owns the windows and table, Arrow holds typed data, and DataFusion runs filters, sorts, and aggregations. The [README](README.md) describes the user-facing workflows.

## Development commands

Run commands from the repository root on macOS with Xcode Command Line Tools and Rust 1.88 or later. Use the checked-in Cargo lockfile.

| Task | Command |
| --- | --- |
| Launch the viewer | `cargo run --locked` |
| Create a demo with nested cells, images, and audio | `cargo run --locked --example make_demo` |
| Open that demo in the viewer | `cargo run --locked -- target/demo/demo.parquet` |
| Check Rust formatting | `cargo fmt --all -- --check` |
| Run the test suite | `cargo test --locked` |
| Run only localization tests | `cargo test --locked --bin parquet-viewer i18n::` |
| Build the release app and disk image in `dist/` | `scripts/bundle-macos.sh --dmg` |

## Where to make changes

- **Window and table behavior.** Start in [app.rs](src/app.rs) for actions, shortcuts, and window state. Use [table_view.rs](src/table_view.rs) for rendering and [table_interactions.rs](src/table_interactions.rs) for row and column actions.
- **Data processing.** [source_loader.rs](src/source_loader.rs) reads Parquet and [csv_source.rs](src/csv_source.rs) reads delimited text. [engine.rs](src/engine.rs) coordinates queries over the typed snapshots in [columnar.rs](src/columnar.rs).
- **Look and feel.** Reuse the controls in [controls.rs](src/controls.rs) and the colors in [theme.rs](src/theme.rs). If a change reaches the vendored GPUI crate, check the [local patch notes](vendor/gpui/PATCHES.md).

## Data and language conventions

- Keep source files read-only. Use [source_guard.rs](src/source_guard.rs) for output destinations, including paths that can resolve to an input through a symlink or hard link.
- Keep original source row numbers attached through range loads, filters, and sorting. Preserve Arrow types and nullability in typed snapshots and Parquet exports. A canceled or superseded background operation must leave the current result usable.
- Write code comments, documentation, and developer-facing instructions in English. Supply both English and Simplified Chinese for application-owned UI text. Use [i18n.rs](src/i18n.rs) and the existing Chinese catalogs. Translate a label or template before inserting a field name, path, or cell value.

## Verify the affected workflow

For reader or export changes, use the [typed Parquet fixture regression](tests/fixtures/README.md) or the [CSV import checks](examples/check_csv_import.rs). Compare types, values, and source row numbers. For UI changes, exercise the action on the demo through its normal menu, shortcut, or table interaction. For localized controls, switch languages with the relevant panel open. For documentation changes, check the linked paths and compare menu labels and screenshots with the current app.
