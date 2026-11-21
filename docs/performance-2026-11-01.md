# Historical Arrow loading, layout, and UI measurements (2026-11-01)

This report covers the earlier DuckDB-to-Arrow loading optimization. Its final build became the baseline for the subsequent Arrow and DataFusion migration. The 9.870 to 3.111 s improvement belongs to this earlier comparison. [Migration report](datafusion-migration-2026-11-01.md)

The Apple Silicon release UUID for this report was `967F582A-969E-3E1D-8700-4C90FB612730`. [Machine-readable measurements](load-performance-validation.json)

## What “Preparing table” meant

The previous path created a complete DuckDB result table, then scanned it to produce Arrow buffers. “Preparing table” referred to this second step. Large results could retain both the native snapshot and the Arrow arrays, and the native snapshot could spill to disk.

In the revised path, ordinary types streamed directly from DuckDB into Arrow memory. The display and export snapshots shared the same `ArrayRef` values. The phase label became “Loading data into memory.” With no filter, the known window size provided a real progress percentage. With a filter, the reader reported loaded rows until EOF, avoiding an extra source scan to predict the output size. UNION, complex MAP keys, and extension types of unverified round-trip fidelity still used a native compatibility snapshot to preserve their types and export behavior. These cases could still have two phases.

The main path used Arrow `RecordBatch` values, schemas, shallow projections, shared buffers, and the official FFI. DuckDB handled filtering, sorting, and aggregation through vectorized parallel execution. The stream adapter checked cancellation and updated progress per batch. Each export registered and cleaned up a temporary reader without creating a persistent Arrow IPC copy. Each window could use all available CPUs independently.

## Release builds on the same source

The existing `struct-5gb.parquet` fixture was **5,523,882,977 bytes and 2,600,000 rows**. The baseline executable was `target/perf-before-check-load-all`; the revised executable was `target/release/examples/check_load_all`. Both produced **5,632,191,967 bytes** of Arrow data.

| Measurement | Baseline | Revised |
|---|---:|---:|
| Full query and Arrow load | 9,870 ms | 3,111 ms |
| Native CTAS phase | 2,885.9 ms | 0 ms |
| Sampled peak physical footprint of the full workload | 8,540,723,752 B | 6,435,017,864 B |
| Sampled peak logical size of private spill files | 2,518,712,320 B | 0 B |
| Sampled peak allocated space of private spill files | 2,544,467,968 B | 0 B |
| CPU time of the full workload | 15.995 s | 3.316 s |

For this full load, elapsed time fell roughly **68.5%** and sampled peak physical footprint fell roughly **24.7%**. On a separate 200,000-row fixture, load time went from 284 to 215 ms and sampled peak physical footprint from 1,374,619,208 to 911,656,160 bytes.

The release executables ran serially on the same machine and source with a warm OS file cache. Query timing covered loading only. Memory, CPU, and spill figures covered warmup, full load, cancellation of an active reload, and export of the previous snapshot. Both versions canceled after work began, at different points in their CTAS or streaming paths. The 20 ms sampler could miss brief peaks. Spill figures describe the space occupied simultaneously. A sampled zero gives no measure of cumulative writes, preferences, exports, or other temporary files.

RSS and physical footprint respond differently to macOS compression and resident allocations. Repeated runs sometimes produced higher RSS on the new path. Arrow logical bytes measure a different quantity from either OS metric. The new fetch timer also included reading and decoding the Parquet source, whereas the old fetch timer read an existing CTAS table. The comparison cannot isolate the Arrow converter's contribution. The batch adapter was already a small part of the workload.

Raw logs are in `target/load-performance-before-5gb-final{,-system}.json`, `target/load-performance-after-5gb{,-system}.json`, `target/load-performance-before-small-final{,-system}.json`, and `target/load-performance-after-small{,-system}.json`. To repeat the measurement, use the [profiling script](../scripts/profile-load.py).

```sh
cargo build --locked --release --example check_load_all
python3 scripts/profile-load.py target/release/examples/check_load_all \
  target/large-data/struct-5gb.parquet target/new-load-profile
```

Use a new output prefix. The sampler reads the child process it starts through the public libproc API. The example checks total rows, first/middle/last anchors, nested values, export of the old result after cancellation, and a single-cell slice's lifetime after the Engine is dropped. It checks source size and mtime without recalculating the source SHA-256.

## UI and main thread

The standard 28 px fixed-height table stopped allocating one GPUI list item per row. Scroll state tracked count, height, and viewport, and only visible elements were created. Resizing and toggling sidebars avoided a rebuild of a million layout entries. Wrapped columns still used a dynamic-height list with per-row metadata. Variable heights or focusable items could trigger the measured fallback. [GPUI patch](../vendor/gpui/PATCHES.md)

Automatic widths sampled at most 256 current result rows. Frozen-area height inspected only the prefix needed to fill the viewport. A fix stopped ordinary frozen rows from continually scheduling idle frames. Final release of old Arrow results and Engine instances moved to the background. Wrapped Struct measurements accounted for the icon and spacing to avoid squeezing text.

The Slate/Iris graphite and indigo palette retained lower background contrast and softer striping. Compact buttons, small corner radii, and fine section separators replaced heavy cards, layered outlines, and large shadows. Filters, previews, calendars, input controls, and menus shared a visual hierarchy. Focus, selection, NULL values, and color marks had distinct indicators.

## Validation

- `cargo test --locked --bin parquet-viewer` recorded 207 passed, 0 failed, and 1 ignored test requiring macOS audio output hardware. Formatting and strict all-target Clippy passed.
- Regression checks covered exact native/Arrow values, empty results, repeated exports, export after source removal, recovery from export errors, cancellation, old snapshots, and resource lifetimes. The concurrent Engine/export check passed 20/20 repetitions.
- Production `FixedHeightItems` arithmetic passed 7/7 standalone tests with actual GPUI types. The added vendored GPUI drawing tests could not run offline without `backtrace` and were excluded from those counts.
- Intermediate build `DF49B8F3` was exercised in light and dark appearance. Checks covered filtering, `id=27` at source row 28, table and JSON previews, drag selection and freezing, first/last navigation, sidebar layout, and automatic widths. Nested wrap measurement was then corrected.
- Independent QA loaded all 2.6 million rows in final build `967F582A`. It observed progress of `71680 / 2600000` and `2%`, followed by a UI-reported load time of 3035 ms. First/last navigation and sidebar toggling worked. The filter `id >= 2599998` returned source rows 2599999 and 2600000 in a UI-reported 16 ms. Three wrapped lines of the Struct summary were visible. Disabling wrap restored 28 px rows, and the same-window preview displayed the table above the JSON.

The UI readings were observations from the application. This pass collected no controlled FPS or p95 measurements and did not repeat the full media suite, GPU performance checks, or cold-cache tests. The final distribution and isolated QA bundles passed strict deep signature verification.
