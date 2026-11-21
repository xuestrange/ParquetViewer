# Toolbar commands and Arrow memory use (2026-11-03)

This revision replaced More with Settings on the left and put data operations on the 36-point top toolbar. It also changed how the app stores DataFusion filter and sort indices, releases an unused provider, and exports contiguous selections. It retained source data types, precision, original row numbers, read-only file access, and in-memory Arrow values.

## Toolbar

A labeled Settings button replaced the application icon on the left. The toolbar grouped 16 commands by task.

| Group | Commands |
|---|---|
| Settings | Settings |
| Source and windows | New Window, Open, Open Folder, Reload |
| Working with records | Fields, Filters, Clear Filters, Aggregation |
| Loading and navigation | Load All Records, Load Range, First Loaded Row, Last Loaded Row |
| Inspection and output | Metadata, Export, Help |

Buttons retained their localized tooltips and shortcuts. Filters, Aggregation, and Settings indicated when open. First and Last operated within the current result after filtering and sorting. GPUI text shaping supplied the caption widths. When space was tight, the toolbar dropped captions in priority order while retaining icons and click targets. Settings, navigation, and Help remained accessible. The filename reserved at least 80 logical pixels for display and window dragging. All controls stayed on a single row without an overflow menu.

Settings opened below the left edge of the toolbar. It offered:

- Light, Dark, and Follow System appearance.
- English, Türkçe, and Simplified Chinese in this historical build.
- Default, ISO 8601, and custom date formats.
- Options to always load all records and select all fields when opening a file.
- About and Table & Filter Help.

The menu used the existing Preferences update and persistence path. It scrolled vertically within the window and closed on an outside click or Esc. Changing language refreshed the toolbar and settings. Field names, values, and source data types retained their spelling. The help page described the new entry points.

## Memory and execution

### 1. Retain DataFusion's streamed row indices

DataFusion evaluated filters and stable sorts on shared Arrow arrays and returned a UInt64 index into the base rows. The old path kept all output index batches, then called Arrow `concat` to build one full array. The source chunks and the combined array coexisted during concatenation. For N output rows, the combined array alone needed approximately `8 × N` bytes plus alignment, validity, and object overhead.

The new `RowIndices` retained the output `Vec<UInt64Array>` and a small prefix count per chunk. Random cell access used prefix counts to locate a chunk. Batch export iterated over contiguous slices of those chunks. The implementation avoided per-row objects, JSON, and a second table of detail values. Arrow `null_count` and `max` checks rejected NULL or out-of-range entries before publishing a snapshot. Canceled and failed queries left the previous snapshot intact.

The `8 × N` estimate applies to the eliminated logical array. Actual RSS also depends on shared buffers, allocator retention, and temporary sorting state. Base indices continued to map to original source positions. DataFusion retained responsibility for the stable sort order.

Multiple output slices can share a single Arrow allocation. The accounting used public `Buffer.data_ptr()` and `capacity()` to deduplicate value buffers and all-valid NULL bitmaps, with one UInt64Array object counted for each nonempty chunk. This measures retained Arrow objects and buffers. OS RSS, physical footprint, and disk space are separate measurements.

### 2. Release the provider after clearing filters and sorting

Previously the app cleared the DataFusion context after special INTERVAL sort keys had been used. Clearing ordinary conditions could leave a complete identity index in the window cache. Queries without filters or sort keys now released the provider, its identity index, and auxiliary sort keys. Loaded Arrow values remained available. A later filter or sort rebuilt the provider from RAM.

Rebuilding the context had a small cost. In exchange, an idle window retained one fewer index. The Tokio runtime, independent per-window CPU policy, and loaded data remained available.

### 3. Export contiguous indices with zero-copy slices

Export projected fields first, then processed row indices in bounded batches. A contiguous range within one source RecordBatch could use `RecordBatch.slice` directly, eliminating new value and offset buffers. Sparse, cross-batch, and reordered rows used Arrow interleave to preserve order and nested types. Temporary address metadata was bounded by batch size.

A unit test verified that projected, reordered fields retained the original Arrow value-buffer pointers when sliced. That check establishes sharing for this path. Export time and cumulative disk writes require separate measurements for each format.

## Other processing paths

Parquet loading already decoded selected roots and requested row groups in parallel directly into Arrow RecordBatches. DataFusion already handled filtering, sorting, and aggregation. Aggregation reused snapshot arrays for projection, replay of pure filters, parallel aggregation, and streaming results. Unused large Struct and Binary columns required no secondary value table. Registering, exporting, and importing another copy would have added memory use and complexity.

Parquet and JSON exports already wrote batches. XLSX used constant-memory mode and temporary XML files to bound RAM. Cancellation, failure handling, atomic destination replacement, and source protection continued through their existing paths. The writer preserved the app's JSON and Excel representations without retaining an entire XLSX file in memory.

All selected Arrow values still occupied RAM. Viewport rendering reduced formatting and painting work. The same-range cache for hidden fields avoided disk reads when users reselected fields. Clearing this cache would trade lower retained RAM for extra reads, so this release kept its existing policy. Exact `nunique` state and high-cardinality grouped results could still grow with cardinality. The per-window DataFusion workspace did not cap total process RSS or coordinate a global budget across windows.

## Validation

- `target/toolbar-memory-tests.log` recorded **213 passed, 0 failed, 1 ignored**. The ignored test required audio output hardware.
- Formatting and strict all-target Clippy passed. See `target/toolbar-memory-fmt.log` and `target/toolbar-memory-clippy.log`.
- The fixture report referenced by `target/toolbar-memory-fixtures.json` recorded 8 passed, 0 failed, and 0 skipped. It covered 14,703 rows, 156,344 cells, and 716 checks of bidirectional sort order and source row numbers. Parquet round trips were exact and the sources were unchanged.
- Ten typed filter checks in `target/toolbar-memory-typed.json` and eleven Struct/List checks in `target/toolbar-memory-nested.json` passed.
- `target/toolbar-memory-aggregation-debug.json` recorded three successful aggregations on 200,000 rows in four groups and validation of a filtered load range. These were debug correctness checks.

Focused index tests covered uneven and empty chunks, repeated and nonmonotonic base indices, chunk and source-batch boundaries, reordered fields, original row numbers, rejection of NULL and invalid indices, empty results, shared pointers on contiguous export, and counting shared value and NULL buffers once. An existing Engine test exercised filtering and clearing after removing the source and verified that the ordinary provider was released. The cross-batch indexed export order regression also passed. See `target/index-chunks-focused-tests.log`.

### GUI checks

Debug build `D1522D90` was checked at the default width of 1280 logical pixels in Simplified Chinese and English, light and dark appearances. QA examined the toolbar and settings menu and restored the original Simplified Chinese and Follow System preferences.

Native window dragging failed to change the window size during the check. A separate debug build changed only the initial width to 1000 px (UUID `AF8D9BDE-A455-3D3C-9E46-9AFFA85672CF`). Its Retina screenshot was 2000 px wide. QA inspected all 16 toolbar commands, the settings menu, and the right-hand window controls in English and Simplified Chinese. No control overflow or overlap was observed. The remaining UI code matched production. The temporary wrapper and app copy were removed after the build. The frozen production source hashes were unchanged, and the special launch source was retained at `target/toolbar-narrow-startup-qa.rs`. The release kept its default 1280 px width. The release startup check is recorded below.

## Benchmarks and delivery

The baseline source and executable were archived at `target/toolbar-memory-baseline-20261103`. The SHA, build log, and wrapper provenance for the standalone baseline `check-index-memory-before` are in `target/index-memory-before-provenance.json`. The matching workload is `examples/check_index_memory.rs`.

The dedicated source `target/demo/index-memory.parquet` contained 10,000,000 UInt64 identifiers and occupied 20,011,858 bytes. Data generation ran separately from measurement. Each independent process loaded the source, filtered on `id >= 1`, applied a stable descending sort, and cleared filters and sorting. It retained the current snapshot and released the old result. Boundary values and original row numbers were checked at each stage. Verification of all 32,345 exported records was outside the timed section. Source size and mtime remained unchanged. The OS page cache was uncontrolled.

Three preliminary baseline runs in `target/index-memory-before-{1,2,3}.json` served as measurement preparation. Each phase recorded query-only time, publication and old-result release time, before/after RSS and physical footprint, and sampled peaks at nominal 5 ms intervals. Short peaks could be missed. Spill samples measured concurrent storage. Allocator retention and compression can delay changes in RSS or physical footprint after buffers are released, so interpret OS observations alongside the ownership tests.

### Alternating release comparison

The final app UUID was **`A4D46258-8B09-3D17-941D-F23CF4D3D6D5`**. File hashes matched the frozen sources at the time of validation. The manifest `target/toolbar-memory-source-freeze.json` had SHA-256 `cabc2920f5072c31ae4a9745958f24d686c3ef006610637af3bf76217ee25024`. The separately built revised example used the same frozen source and had SHA-256 `3e3e6f1bfeb1ff5e762caa5ba36667d405d5a1feed99bb8333edf86a680d82a7`.

The three independent process pairs ran **BEFORE/AFTER, AFTER/BEFORE, BEFORE/AFTER**. Each performed the same load, broad filter, descending sort, and clear. Each phase below uses its three-run median. Preliminary runs and debug timings are excluded. Raw runs are in `target/index-memory-paired-before-{1,2,3}.json` and `target/index-memory-paired-after-{1,2,3}.json`. [Full comparison](../target/index-memory-comparison-20261103.json)

| Measurement, median of three processes | Before | After | Change |
|---|---:|---:|---|
| Broad filter, query only | 122.518875 ms | 115.629750 ms | Down 5.62% |
| Descending sort, query only | 267.430166 ms | 251.126084 ms | Down 6.10% |
| Physical footprint after filter publication | 344,294,144 bytes | 265,061,072 bytes | Down **23.01%** |
| Observed peak physical footprint across four phases | 816,759,912 bytes | 743,212,064 bytes | Down **9.00%** |
| Observed peak RSS across four phases | 840,892,416 bytes | 767,197,184 bytes | Down **8.76%** |
| Clear, query only | 0.032667 ms | 0.275625 ms | Up 0.242958 ms |
| Physical footprint after clear publication | 816,727,144 bytes | 743,195,680 bytes | Down 9.00% |

Query-only timing excluded formatting for correctness checks and export. Publication and old-result release were timed separately. For each process, the observed peak was the maximum of before-phase, after-publication, and periodic OS samples. The four-phase result takes the maximum across the process's phases, then the median across the three processes. Each sample required libproc and workspace-directory inspection in addition to the nominal 5 ms sleep. These sampled maxima can miss short peaks and cover only the measured phases.

All six runs produced the same row counts, boundary values, and source row numbers. Each also checked all 32,345 sorted export values. Source size and mtime agreed across the six runs. Both versions recorded **zero sampled spill** in the private query workspace. The comparison measured no reduction in query spill space and did not measure all application writes.

The loading implementation was unchanged. Baseline load times were 147.183917, 15.980000, and 21.147750 ms. Revised load times were 22.667583, 15.082833, and 16.279041 ms. The effects of page cache and system state were not isolated from code differences. No compilation or root-initiated GUI workload ran during the comparison. External desktop activity, OS page cache, and overall memory pressure were uncontrolled and were not continuously monitored. An existing QA process was running before the comparison and had exited by the end. Its exact exit time and activity during the runs were unknown. The 5–6% timing difference describes these local observations.

The clearest observed benefit was about 79.2 MB less physical footprint after the broad filter. Chunked indices required additional object metadata. Logical Arrow accounting after filter/sort went from 160,136,760 to 160,166,040 bytes, an increase of **29,280 bytes**, while process footprint fell. Releasing the provider added about **0.243 ms** to clear. The next filter rebuilt the context without rereading Parquet. Post-clear footprint was still about 743.2 MB, well above the approximately 80 MB of original Arrow values. Retained allocations, the runtime, and compression affect that reading. The narrow fixture magnified index costs relative to wide Struct or Binary data.

| Deliverable | Recorded status |
|---|---|
| Release UUID, frozen source, signature | `A4D46258-8B09-3D17-941D-F23CF4D3D6D5`, matching source hashes, strict deep signature verification passed |
| Paired timings and resource use | Three serial alternating pairs completed. The comparison includes raw runs, exact medians, and ranges |
| Bundle and installed files | App 101,814,592 bytes, ZIP 34,517,079 bytes. The extracted signature was valid and installed files matched |
| Release GUI check | The isolated QA bundle and the installed app launched with the same code UUID |

To reproduce the example:

```sh
cargo test --locked --offline --bin parquet-viewer
cargo fmt --all -- --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline --release --bin parquet-viewer --example check_index_memory

target/release/examples/check_index_memory --make
target/release/examples/check_index_memory target/demo/index-memory.parquet
```

Use `--make` only when the dedicated input does not exist. Fixture generation runs outside the timed comparison.

## Final application and installed release

The independent QA release opened `aggregation.parquet`, applied nine fields, and loaded all 200,000 rows from the toolbar. The UI reported 19 ms. Filtering on `category=华东` returned 60,000 rows in a displayed 25 ms, including original rows #1, #4, #7, and #13. Count rows in the aggregation sidebar produced one group with a count of 60000 from those 60,000 detail rows in 16 ms. The main table continued to show details. Clear Filters restored 200,000 rows. The toolbar opened Metadata and an export prompt for 200,000 rows. New Window opened window 2, and closing it returned to the original data window. These UI timings were separate from the CLI performance comparison.

After QA exited, the release was installed at `/Applications/Parquet Viewer.app`. Per-file SHA values matched the distribution bundle and strict deep signature verification passed. The previous copy was retained at `target/installed-app-backups/20261103-185608/Parquet Viewer.app`. The installed application launched. Opening and closing Settings confirmed the new toolbar and settings groups.

Regular files in the app totaled **101,814,592 logical bytes (97.10 MiB)**. The ZIP was **34,517,079 bytes (32.92 MiB)**. ZIP CRC, executable/Info.plist equality, and strict deep signature checks on the extracted app passed. Raw results are in `target/toolbar-memory-package-final.json`, `target/toolbar-memory-archive-roundtrip.json`, and `target/installed-release-20261103.json`. [Validation summary](toolbar-memory-validation-2026-11-03.json)

This pass did not rerun the full media suite or 5.5 GB GUI load. The isolated QA instance exited. Simplified Chinese and Follow System were restored. Power settings were unchanged and caffeinate was unused. Production sources remained frozen across tests, the release build, and measurement.
