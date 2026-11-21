# Data behavior and historical validation

Parquet Viewer uses Arrow and DataFusion to process data. The [design document](nested-and-import.md) covers nested data, loading, columnar storage and source protection. Build identities, test results and timings below apply to their respective versions.

## Type-aware filtering

Field selection, shortcuts, operator labels and input validation use the same schema type classification. A text value such as `00123` retains its type. Numeric inputs preserve Decimal and large-integer precision. Date supports on/before/after dates and inclusive ranges. Timestamp and Time compare complete values. The Date/Timestamp calendar changes only the date, preserving any time, fractional seconds and explicit zone. An empty Timestamp starts at midnight, with `+00:00` for zoned types. Struct fields use the type at their schema path. Primitive Lists test typed membership against complete elements, with case-sensitive Text matching and separate checks for empty collections and NULL. The following section covers complex Lists, Map and Binary.

Controls read existing field metadata without scanning data. The implementation described here loaded an Arrow base for the source range and field projection, then filtered and sorted it in RAM. Reusing that base across requests did not push the condition into the Parquet decoder. Greedy CPU allocation and original row numbers were retained. The earlier typed-controls build `B0CD9CDF-E6FC-393D-B61B-3034D01FBADB` passed 198 tests and 10 real typed queries. Initial GUI checks for numeric, Date, Boolean and VARCHAR fields used a build with UUID prefix `5855`; Time controls were also checked. Separate checks on the final corrected package covered leading/trailing whitespace for Timestamp_NS, calendar positioning, retention of 9 fractional digits, an actual single-row match, List NULL controls, and a single-line format hint. See the type-aware filtering section of the README for that release for details and exclusions. The earlier 5 GB test was not rerun.

## Struct, List and in-window preview

Struct targets are expanded from the existing schema and cached. A condition addresses a root field and separate path segments. Names containing dots, quotes or spaces retain their identity. Each leaf uses comparison and input controls for its own type, while the Struct itself supports NULL checks. Rendering and draft editing require neither recursive schema parsing nor a scan of column values.

Primitive Lists support contains/does not contain an entire element, empty/nonempty collections and NULL/non-NULL checks. Number, Boolean, Date and Timestamp use exact typed literals. Text elements use case-sensitive whole-value matching; ordinary text fields retain their substring filtering rules. An empty List differs from a NULL List. Lists of Structs, Maps, Lists and other unsupported element types offer collection-emptiness and NULL checks. Map, Binary and unrecognized fields support NULL checks.

Preview shares the main window. Like Filters, it is 360 logical pixels wide, with a single-line heading, Close button, consistent padding, spacing and button styles. The truncated type summary has a full tooltip. The nested table and JSON occupy roughly 55:45 of the available space, and images retain their aspect ratio. Filters and Preview are mutually exclusive; switching preserves the filter draft. Close or Esc releases the panel's entities, media and Arrow references. Changing the source or snapshot closes the old preview, while reapplying an identical request keeps it open. Copy Node, Copy Cell, Save Value, copying/saving images and nested navigation remain available.

Build `E2AEC0C9-2366-38AB-9A60-8921798F172E` passed 202 automated tests and the 11 real queries in `target/nested-filter-validation.json`. Unit regressions covered another 25 combinations of NULL, empty collections, precision and special field names. The README for that release separately records the initial `975C37EA` joint filter on an exact-decimal Struct child and a List, the `EFBA` 360px/Close checks, and the final `E2AEC0C9` retest. Final GUI checks covered original row numbers for List contains 28, empty/nonempty labels, unapplied drafts when switching between Filters and Preview, paste verification after Copy Cell, Struct table/JSON, Photo SVG, Esc and reload closing the old preview. The full 5 GB, theme and media suites were outside that run. The user's original app stayed running. Records for the older separate quick-preview window and Lists limited to NULL checks apply to those builds only.

## Filter and query scope

The filter sidebar builds typed conditions. The data path in this version was:

```text
Parquet partitions → field and source row-range selection → Arrow base in RAM
                                                              ↓
                                    DataFusion filter / global sort → UInt64 row-index view
                                                              ↓
                                                display / snapshot export
```

The sidebar generates typed DataFusion expressions. Struct fields use distinct path segments, and Lists match complete elements in their original type. Decimal and large integers avoid intermediate Float conversion. Date, Time and Timestamp input is validated against the relevant Arrow type. Ordinary Text filters and sorting ignore case, while Text List membership is case-sensitive. Equal sort keys use base indices to maintain stable source order; NULLs sort last. Map and nested Map sort keys adapt only the container metadata over shared buffers to a list of key/value structs. Source data remains Map. The sidebar accepts neither arbitrary SQL nor aggregate expressions and does not implement the full .NET DataView interface.

## Filtering large datasets

Source metadata locates the requested row range and skips disjoint partitions and row groups. The Parquet reader projects columns and selects rows. A per-window Rayon pool reads intersecting row groups in parallel, and the merge restores source order. Some encodings may still require decoding skipped values. Parallel groups do not guarantee full CPU utilization for every file.

In the implementation described here, filtering and sorting operated on the unfiltered Arrow base after import. Predicates were not pushed into the Parquet scanner. Subsequent operations reused the base for the same source, fields and Offset/Rows. Compatible query shapes reused the DataFusion context. Returning UInt64 base indices avoided copying all result values. Switching the legacy INTERVAL sort target rebuilt just the context and its comparison key; clearing that sort released the key. Changing the fields or range, or explicitly reloading, reread the source. A selective result still retained all requested window values. This avoids repeated interactive reads and decoding; initial selective filters can still cost more than a pushdown scan.

Row numbers identify source positions. Single files start at `1`; partitions accumulate row counts in the sorted order of discovered files. Filtering retains gaps and sorting preserves each record's original number. The query/Arrow boundary keeps hidden source identities separate from user data. Copy and the four export paths in this version neither add a row-number column nor mask identically named user fields.

Reapplying skipped work only when the source appeared unchanged and the request matched the displayed one. Reload forced a new read. Repainting the sidebar compared applied-condition state without recloning schemas or recompiling expressions. Total and remaining rows for the load range came from existing metadata without an extra scan.

## Partition schemas and logical types

Partition merging compares actual Arrow DataTypes, field names and ordering. It recursively merges nullability in Struct/List/Map and other containers. Accepted common representations are demonstrably lossless, such as Null with a concrete type, compatible Binary storage, increased Time precision and Float16→Float32. Arrow cast performs conversions. Overflow, incompatible types or incompatible extension metadata produce explicit errors instead of NULL values. Identical `BLOB`, `TIME` or `FLOAT` UI labels do not guarantee identical storage. Schema-level metadata comes from the first partition; the other partitions retain their own summaries. Arbitrary schema evolution and lossy coercion are outside this contract.

Canonical Parquet extensions identify JSON and UUID from logical annotations even without an embedded `ARROW:schema`. Field metadata is preserved recursively at the top level and in Struct, List and Map keys/values. JSON export preserves the tree and exact integers. UUID uses canonical text for display and JSON. Its original bytes are retained, and the Parquet writer preserves the logical annotation. Shared physical storage alone does not allow JSON/UUID partitions to merge with ordinary VARCHAR/BLOB.

Legacy Parquet INTERVAL keeps all 12 bytes as `FixedSizeBinary(12)` with logical metadata. Choosing a single Arrow Interval unit could discard other components. Display, preview and JSON/Excel preserve legacy canonical text such as `1 month`, `30 days` and `00:00:00.001`. JSON uses a String. Parquet export keeps the bytes and restores the INTERVAL annotation. Only a selected sort field containing legacy INTERVAL triggers lazy generation of a Decimal128 comparison key using month=30 days and day=24 hours. The original month/day/millisecond values remain intact. Ordinary filtering, sorting other types and unsorted paths do not scan INTERVAL columns or allocate these keys. JSON, UUID and INTERVAL retain NULL-only value filters.

## Large results and progress

Imported values reside in system memory as a typed Arrow base. This design removed the previous fixed 128 MiB per-result total. Two-axis virtualization limits UI elements and GPU drawing, while retaining the base arrays. Filter and sort views share values with the base. Helper or result indices, sort keys and query workspace consume additional memory. A UInt64 index uses roughly 8 bytes per row for its values, with possible temporary buffers during execution and concatenation. Explicit copying, nested JSON, media and export writers also allocate. DataFusion operators supporting spill may use temporary disk without evicting the resident Arrow base.

Read progress divides decoded rows by the window total known from metadata. Worker callbacks contribute to one monotonically increasing count. DataFusion then shows separate filtering or sorting stages. When the output count or operator completion is unknown, the UI shows the stage and emitted rows, without treating them as scanned rows or inventing an overall percentage. With no filter or sort, preparation simply publishes a shared snapshot of the base. Cancellation or failure preserves the old result. Changing conditions on the same window enters the query stage without replaying file-read progress. The toolbar provides range and full-load actions.

## Date and binary display

An unquoted, unescaped `%` in a custom format selects chrono/strftime, for example `%Y-%m-%d %H:%M:%S`. Other formats support common .NET patterns such as `yyyy-MM-ddTHH:mm:ss.FFFFFFF`.

The .NET subset supports `y` through `yyyyy`, `M` through `MMMM`, `d` through `dddd`, one or two `H/h/m/s` characters, one to seven `f/F` characters, `t/tt`, `z/zz/zzz` and `K`, with single- or double-quoted literals and backslash escapes. `F` drops trailing zeros and, if all are zero, the preceding dot. .NET patterns show up to seven fractional digits; ISO/chrono can preserve nanoseconds. Adjacent quoted literals concatenate per .NET rules. Use a backslash or the other quote delimiter for a literal apostrophe.

The supported calendar is Gregorian, years 1–9999, with fixed English month, weekday and AM/PM names. Separators do not depend on system culture. Eras, standard single-letter date formats and .NET's `%` single-specifier shorthand are unsupported. Without a source time zone, `K` is empty and `z` does not infer the machine offset. Invalid or unsupported patterns and missing date/time components report conversion failure without panicking on the render thread. [Microsoft's custom date/time format documentation](https://learn.microsoft.com/en-us/dotnet/standard/base-types/custom-date-and-time-format-strings)

Binary numbers use little-endian order. GUIDs use .NET mixed endianness and IP addresses use network byte order. Empty byte arrays and null differ. Display conversion leaves source files and underlying export values intact.

## Columnar reads, rendering and resources

The Parquet reader produces typed `RecordBatch` values directly. `ArrowTable`, DataFusion MemTable and immutable snapshots share the same `ArrayRef` buffers. Without filter or sort, the view reuses the base. When processing is needed, DataFusion emits UInt64 base positions and the display looks up values through that mapping. Production results do not store `Vec<Vec<Cell>>` or precompute strings/JSON for the entire table. There is one query engine and no native compatibility result table. Unsupported conversion or export fails explicitly; old compatibility tests cannot establish support for every Parquet logical type.

The complete requested window remains resident. The previous independent 128 MiB Arrow result rejection threshold was removed. Loading more rows or fields increases resident memory. `Cell`, text and JSON are produced on demand. Ordinary display is limited to 2000 Unicode characters; copy, save and export retain full values. The per-window `SharedString` LRU cache has 8 MiB while active and 1 MiB while inactive. Windows do not share that quota.

The table virtualizes both axes with a 120px horizontal buffer. Frozen panes determine their own visible ranges. Fixed-height rows require constant-size scroll metadata, create only visible elements, and avoid full-result layout rebuilds on resize or sidebar changes. Wrapped columns retain estimated heights and a dynamic-height cache with per-row metadata. Auto-width uses at most 256 stratified samples. NULL highlighting reads the Arrow bitmap without guessing from text or scanning the table. These choices reduce element creation and formatting without changing query range or exports.

Each window uses all `available_parallelism` for its Parquet Rayon pool, DataFusion Tokio workers and target partitions. This version has no one-core reserve, 16-core cap, global two-operation limit or shared CPU/memory pool. Per Engine, the DataFusion `GreedyMemoryPool` allowance is worker count × 256 MiB, bounded to 2048–4096 MiB. Spill-capable operators use a private workspace. This allowance does not bound resident Arrow data, and some operators cannot spill. Multiple windows contend for CPU, RAM and disk; base data, indices, caches, JSON, UI, media and writers add to process memory. The configuration sets neither a hard RSS cap nor a guaranteed CPU utilization.

## Export and source protection

Nested-table virtualization limits the number of visible elements constructed. Opening a structure materializes the cell's JSON and `NestedTable` in the background; column discovery and copying the whole table may still traverse all children. The image-specific preview skips JSON. Explicit value copying borrows a BLOB's complete bytes from a shallow one-cell Arrow slice and uses the BinaryFormat captured when opening the preview. Photo STRUCT keeps complete JSON. A slice may retain buffers for its column in its source batch, but not other columns or batches. Pretty JSON below the table is generated and cached on open and navigation, without per-frame serialization. Its 256 KiB display budget includes the truncation notice. Truncated preview text need not be a complete valid JSON document. Copy Node and Save Value use complete content and may allocate more. See the [design document](nested-and-import.md) for detailed memory and preview boundaries.

CSV, JSON, XLS, XLSX and Parquet use the current immutable Arrow snapshot and its field order. Contiguous ranges share slices. Other ranges gather rows in batches of up to 8192. CSV uses UTF-8, empty fields for NULL and compact JSON for nested values. JSON writes one cell at a time. XLSX uses constant-memory worksheets and temporary storage; XLS retains SST/BIFF buffers. Freezing affects display only: exports follow query order. Every format creates a new temporary file with `create_new` in the destination directory. After success, cancellation and source protection are checked before rename. Compression or work inside the writer can defer cancellation to a batch boundary.

Maps with ordinary unique keys become objects in JSON/Excel. When formatted keys collide or repeat, an array of entries preserves their original types and values, e.g. `[{"key": original_typed_key, "value": value}, …]`. Nested Maps use the same rule. Copying preview JSON gives the display representation and can differ from exporting the raw snapshot. Parquet export preserves the original Map container and keys/values, excluding sort helpers.

Legacy `.xls` allows 256 columns, 65,535 data rows plus a header, and 32,767 UTF-16 units per text cell. SST/CONTINUE long strings and FAT/DIFAT sector tables remove earlier implementation-specific restrictions on short text and small workbooks. The CFB v3 limit of about 2 GiB and available memory still apply; exceeding bounds produces an explicit error. XLSX checks Excel's row/column limits and writes values beyond its reliable numeric precision as text. JSON/Excel do not preserve every Parquet logical annotation. Type mappings in the historical table-creation script also could not express all logical types losslessly. That feature has since left the UI.

Parquet footers supply source row counts, schema, projections and pruning metadata. Sources remain read-only. Exports and preferences are written as separate derived copies and application files.

## Scope of historical validation

The remaining UUIDs, test counts, UI entry points and measurements refer only to their respective older builds. Statements about the old query engine, text import, CSV export, additional load commands and the 128 MiB resident-result limit describe historical behavior. The migration report defines the validation scope for the Arrow + DataFusion architecture.

## Historical random paging in text files (feature removed in a later build)

The historical package passed **169 tests with 0 failures and 1 audio hardware test ignored by default (170 total)** under `cargo test --locked`. Formatting, strict Clippy (`--locked --all-targets -- -D warnings`), optimized packaging and strict deep signature verification of dist and independent QA copies passed. All three release/dist/QA UUIDs were `E262D7C4-5243-33B8-8F61-763B9BFE758B`.

Independent QA on that final release covered indexed text paging. Native Open loaded `target/demo/text-index-qa-1793447584008068000.records` (406,974 bytes, 9007 rows, 4 fields) and sampled 1000 records. The wizard showed a pipe delimiter, headers, Chinese text, a quoted newline and decimal NULL. The initial window displayed 1000 rows starting at id0, total unknown, and 6ms on the UI. More → Load Range with offset9000/Rows1000 returned 7 rows, id9000–9006, NULL for id9002 and an exact total of 9007 in 10ms. Jumping to offset4096 showed 1000 rows with the total still 9007. Returning to offset0 again showed 1000 rows starting at id0; both jumps displayed 8ms. These labels describe a small fixture, outside the scope of 5GB GUI or controlled performance tests.

⌘Q exited the independent QA app. The original app was neither opened nor restarted. No system settings or caffeinate changes were made. The earlier wide-table, NULL, image and full media GUI suites were not rerun. A separate standalone check compared 60 random/boundary jumps against full CSV decoding for each of UTF-8, UTF-16LE and UTF-16BE, over 12,553 rows including Unicode and mixed newlines. It is separate from the Cargo test count.

A local release example passed a single regression on a **5,468,522,249-byte / 2,600,000-row** pipe-delimited `.records` file. Prefix inference took 59ms, opening took 26ms. The reports are `target/text-random-pages-1793447530360820000/report.json` and `target/text-random-resources-1793447530360823000.json`.

| Request | Offset / rows | Time | Bytes framed for this request |
|---|---|---:|---:|
| First 1000 rows | 0 / 1000 | 24ms | 2,096,469 |
| First unindexed last page | 2,599,000 / 1001, returned 1000 | 4658ms | 5,466,425,780 |
| Indexed random page 1 | 1,300,000 / 1000 | 26ms | 5,403,252 |
| Indexed random page 2 | 17 / 1000 | 23ms | 2,132,131 |
| Indexed random page 3 | 2,599,000 / 1000 | 26ms | 6,598,368 |
| Indexed random page 4 | 500,000 / 1000 | 23ms | 2,707,652 |
| Indexed random page 5 | 2,000,000 / 1000 | 22ms | 4,527,948 |
| Indexed random page 6 | 1,300,000 / 1000 | 23ms | 5,403,252 |

The on-disk index was 32 bytes for the first window and **20,320 bytes** after the first jump to the last page. EOF established an exact count of 2,600,000. Empty pages beyond EOF did not rescan. “First unindexed” refers only to index construction. The **OS cache was not cold**. Framed bytes count the logical record framing range, excluding other process I/O and scan-buffer read-ahead.

Checks covered page values, quotes, NULL rules, export from the old snapshot after random jumps and source-overwrite prevention. Source SHA-256 was `81d8c96f253c6a1eb186a7e89c60b70c263b5208f7906f7751faa52262636dc3`; device/inode, size, mtime and ctime matched before and after. The `wait4` harness recorded a maximum RSS of **49,348,608 bytes (47.0625 MiB)**. Wall time of approximately **37.77s** included two full SHA-256 passes. It measures neither query latency nor GUI memory. The run was not a controlled comparison with the earlier unindexed debug build's 51.698s. FPS, p95 and CPU profiles were not collected and the results do not establish lower RSS.

The [validation JSON](large-file-validation.json) records this evidence under `text_random_paging`. Earlier sequential-offset, Arrow migration and copy-fix measurements retain their original UUIDs and scope.

## Previous Arrow columnar storage and greedy resource allocation

The final package of this version passed **160 tests with 0 failures and 1 audio hardware test ignored by default (161 total)** under `cargo test --locked`. `cargo fmt --check`, strict Clippy (`--locked --all-targets -- -D warnings`), optimized packaging and strict signature checks passed. Final release/dist/QA executable UUID: `982A499C-78E0-37F0-AB48-D67553F55204`.

The full wide-table/NULL/freeze/nested/multi-window GUI baseline was `119E7A1E-3768-390C-BDEB-76F7E07EE523`. It passed **159 tests with 0 failures and 1 ignored (160 total)**, plus formatting, strict Clippy, release build and strict signing checks. The final build changed only the image copy-value paths in `app.rs` and `peek.rs`. It had a dedicated copy smoke test rather than another run of the full GUI checks below.

A single headless debug run on the local machine recorded Arrow array memory and query time in `target/arrow-storage-check-53407-1793444884249150000/report.json`. It used 8 available workers. `get_array_memory_size` measures retained array buffers and metadata, **not process RSS, peak physical footprint, GUI FPS or a controlled speedup**.

| Requested result | Arrow array memory | Query time |
|---|---:|---:|
| NULL fixture, 100000 rows × 8 columns | 5,823,733 bytes (5.554 MiB) | 266 ms |
| Wide table, 1000 rows × 1000 columns | 9,385,464 bytes (8.951 MiB) | 671 ms |
| Wide table, 5000 rows × 1000 columns | 46,975,871 bytes (44.800 MiB) | 1253 ms |

Additional checks covered exact decimals, integers above u64, NULL bitmap vs. literal text, nested values, and two Engines that started together at a barrier, each filtering 10 rows and exporting an older snapshot. BLOB and Photo STRUCT images skipped JSON construction. The deferred copy's one-cell slice shared binary buffers. SHA-256/mtime remained unchanged for all four sources. The 5000 × 1000 fixture was 10,059,223 bytes. This check does not establish RSS or frame-rate improvements or validate every large-data scenario.

Full GUI checks for release `119E7A1E-3768-390C-BDEB-76F7E07EE523`:

- Blue window 1 opened the wide table with 1000 fields and 1000 initial rows. More → Load All loaded 5000 rows in 248 ms on the UI label, separately from the debug timings above. ⌘End reached the last row with `LAST · row=4999 · col_0999` in `col_0999`. ⌘Home returned to the start.
- Column 0 and the first two rows were frozen after dragging the row numbers. At ⌘End, ids `0/1` stayed at the top. The last row's left-hand id `4999` and right-hand final-column value matched, and the auto-width menu was available.
- Teal window 2 independently opened the NULL fixture. Load All increased 1000 rows to 100000 (40 ms on the label). Double-clicking items asynchronously opened a table for `[0,null,1]` above pretty JSON. Save Value wrote the 20-byte `target/demo/cell.jsonarrow-items-qa-119E`, independently read back as `[0,null,1]`. Return located the original items cell in window 2.
- details → records showed Chinese, 😀, a NULL record and a null field, corresponding to the JSON below. Filtering window 2 to `id=1` produced 1 row. In dark mode the NULL in amount used only a distinctive text color with a clear four-sided selection outline. Literal `NULL` kept the ordinary foreground.
- Returning to window 1 preserved 5000 rows, frozen rows/columns and the last-cell selection. Dark mode was synchronized; blue and teal titles still distinguished the windows.

Supplementary GUI checks used the same unchanged release, `119E7A1E-3768-390C-BDEB-76F7E07EE523`. The dark wide table retained 5000 rows and its frozen first column and first two rows. Dragging the horizontal scrollbar from about 2180px to 1440px exposed `col_0494`–`col_0504`. The MIDDLE value in `col_0500` matched the main pane's row id. Frozen ids `0/1` and the top rows remained synchronized without gaps. Purple window 3 opened Photo STRUCT and a `binary_image` BLOB. Double-clicking either showed a real 16,516-byte PNG with the inherited title color. Photo's background Copy Value reported Copied. Raw clipboard contents were not read. Return located the original cell in window 3. After restoring light mode and closing windows 3 and 1, window 2 retained its single-row `id=1` filter and selected NULL. ⌘Q quit independent QA. The original main process was not restarted and no `caffeinate` or system theme change occurred.

The final copy fix and dedicated smoke test belong to `982A499C-78E0-37F0-AB48-D67553F55204`. A BLOB image preview captured `BinaryFormat` on open. Explicit background copying borrowed the full bytes and applied that format, replacing the previous `<binary: n bytes>` summary. Photo STRUCT continued to produce full JSON including path/bytes. New automated regressions covered 13 formats, 3 payload lengths (including 4096-byte data beyond preview truncation) and Photo JSON path/bytes.

The final build loaded all 7 fields of `photo-ui.parquet`. Both `binary_image` BLOB and Photo STRUCT opened a real 16,516-byte PNG on double-click, Copy Value reported clipboard completion and Return located the original cell. The GUI checks only established preview, copy completion and navigation; the automated tests verified full clipboard content. ⌘Q then exited independent QA without restarting the original app, using caffeinate or changing system settings.

This round also passed a **5GB window regression**, documented in `target/arrow-large-validation-1793445591102613000/report.json`. Each source had 2.6 million rows.

| Source | File size | First 1000 rows | Last 1000 rows | Harness maximum RSS |
|---|---:|---:|---:|---:|
| Nested Parquet | 5,523,882,977 bytes | 69 ms | 40 ms | 176,406,528 bytes (168.23 MiB) |
| Pipe-delimited `.records` | 5,468,522,249 bytes | 111 ms | 51,698 ms | 1,073,414,144 bytes (1023.69 MiB) |

The standalone regression used cached debug dependencies to exercise window queries, filter/sort, export and source protection. The final copy fix left the backend unchanged; the evidence covers that same backend, without a further run on the final package. Parquet passed all five exports and source-overwrite prevention. Text passed range filter/sort, export, unknown→exact row count at EOF and overwrite prevention. SHA-256, mtime, size and device/inode were unchanged for both sources and hard-link aliases. **This was a windowed backend test, outside the GUI, and did not load an entire 5GB source into the Arrow result.** Reaching the last text page required scanning prior records sequentially; fast random last-page access was not established.

RSS came from a direct `wait4` child rusage run and included the harness, native allocations and integrity-hashing subprocess scope. The GUI physical footprint and the 128 MiB per-result Arrow array budget use different accounting. Full wall times including whole-file hashes were about 29.84s for Parquet and 83.06s for text, separate from the query times above. The original `/usr/bin/time -l` exited 1 because the sandbox denied access to `kern.clockrate`; the failure log was retained. Both subsequent direct `wait4` harnesses exited 0. Cold caches were not controlled and there was no paired speed comparison.

The listed GUI checks and the 5GB window regression on the text backend without an index were completed for this columnar version. Coverage did not exhaust all media/window combinations or measure FPS/p95, CPU profiles or controlled RSS differences. The [validation JSON](large-file-validation.json) records the scope in `arrow_columnar_greedy`. The older results and UUIDs below retain their original attribution.

## Earlier NULL text color and independent windows

Build `26D95E7F-2160-3FDE-ADF8-78A79A19B01C` used row virtualization, per-cell missing flags and a global two-operation maximum. The later Arrow bitmap, two-axis virtualization and greedy strategy have separate migration validation.

A true NULL used a distinct purple-rose foreground. Normal and alternating rows, hover, selection and row/column marks kept their existing backgrounds and hues. The actual missing flag controlled the color: empty strings, literal `NULL` and numeric `NaN` did not automatically become missing. Explicit missing tokens in text imports used the parsed result under the chosen import rules.

The main table reused each snapshot's `Cell.is_null` within the row-virtualized viewport plus a 120px margin above and below. An O(1) branch selected the color without a full-table scan, per-cell badge, extra UI element or NULL cache. Horizontal scrolling still used clipping, with **no column virtualization at that time**. Every column of a rendered row still incurred layout cost. Highlighting left the requested load range, memory budget and export values unchanged.

The nested quick-preview table used the same foreground while retaining normal or selected backgrounds. The model distinguished JSON null, a missing field and an entirely null record from the string `NULL` or an empty string. The five-row import-wizard preview used the same palette based on parsed missingness.

The menu/⌘N opened independent main windows, each with its own queries, tasks, close and cancel. Finder used a blank window when available. Window creation order selected a fixed title-bar color, and previews inherited it. The first 8 colors favored distinction; later low-saturation colors repeated after 4096 distinct RGB values. The 4096 colors were not guaranteed to be perceptually distinct. Themes and persistent preferences were shared. One application-level heartbeat sampled Memory regardless of window count. Heavy open/import/query/export operations shared two slots. Regressions covered cancelled waits and RAII slot recovery. Each Engine retained independent CPU/memory budgets and caches; these did not cap combined RSS.

The complete `cargo test --locked` run passed **139 tests, with 0 failures and 1 audio hardware test ignored by default (140 total)**. Formatting, strict Clippy (locked/all-targets/-D warnings) and the optimized `scripts/bundle-macos.sh` build passed. The dist and independent QA apps passed strict deep signature verification and shared UUID `26D95E7F-2160-3FDE-ADF8-78A79A19B01C`.

Final QA covered:

- Blue window 1 opened the NULL fixture with 1000 rows. Load All showed 100000 rows and last id `99999`. In light/dark mode only the NULL foreground changed to purple-rose. Alternating, hover and selection backgrounds were preserved; empty strings and literal `NULL`, `NaN` and `null` remained ordinary data.
- Teal window 2 opened an independent demo (5000 total, 1000 loaded); filtering to `id=0` gave 1 row. ⌘+backtick returned to window 1 with 100000 rows and its trailing amount selection intact. Switching window 1 to dark also updated window 2, with distinct title colors.
- The items preview in window 1 showed three rows, `[0,null,1]`. Selected and unselected null both used only the dedicated text color; the preview inherited the blue title. ⌘N from the preview created purple window 3 in dark mode. ⌘W closed just window 3 and returned to the preview. Return located the original items cell in window 1.
- Closing window 1 returned to window 2 with its single filtered row intact. Window 2 was restored to light with a teal title. ⌘+backtick stayed there after closing the other window. The native Window menu listed only demo window 2, confirming cleanup of window 1, its preview and window 3.

This round did not measure FPS/p95/CPU profiles, rerun 5GB tests or perform GUI checks of import preview, actual external Finder routing or the complete media suite. Finder routing had source and automated checks only. See `null_text_only_multiwindow` in [large-file-validation.json](large-file-validation.json). The A3/128 and earlier records below apply to their original releases.

## Earlier experimental NULL background

This experiment passed **128 tests, with 0 failures and 1 ignored by default (129 total)**, plus formatting, strict Clippy, release and strict signing checks. Release, dist and independent QA UUIDs matched: `A3F2F481-B690-3A6D-8CD5-07421A9655C2`. Unselected, unmarked NULLs had a light background at that time. The user subsequently requested text-only highlighting, so the previous background checks apply only to that experiment.

Independent QA (`org.parquetviewer.feature-qa`) used native Open to load `null-density.parquet` with 100,000 rows × 8 columns. The first 1000 rows displayed a rose foreground and light background for true NULL, while empty strings and literal `NULL`/`NaN`/`null` kept their regular style. Load All showed 100000 rows. Scrolling and CmdEnd updated NULL highlights with the viewport. A selected NULL at the end in dark mode kept the selection background and rose text. After returning to the top, Blue/Teal/Amber row marks and a selected Amber row still showed legible NULL. The dark null in the `[0,null,1]` preview, a selected null after switching to light, Return, and both a NULL record and a null field under `details.records` were verified. Fixed light mode was restored.

The fixture was 228,638 bytes, with 578,834 top-level NULLs in 800,000 cells (72.35425%). The estimated engine display budget usage was 104.50 MiB, **not a process RSS measurement**. There was no additional manual import preview, FPS/p95/CPU profile, 5GB retest or restart of the original main process. The `missing_value_highlighting` record refers to the background experiment.

## Earlier system theme, selection and memory status

More offered Light/Dark/Follow System, and the native Edit menu showed the current theme mode. Fresh installs defaulted to System. Migration from the old `dark` Boolean preserved the explicit choice. System mode responded to appearance events and updated main windows, existing inputs (including filter/import) and open previews. Fixed modes retained the user's choice. Selections gained a 2px painted outline without affecting layout; individual cells, rectangles and regions crossing frozen panes were outlined separately. The Memory status used macOS `proc_pid_rusage` to sample current `ri_phys_footprint` in the background every 2 seconds. A failure displayed `—`. Rendering did not call the system API per frame.

The full suite passed **124 tests with 0 failures and 1 audio hardware test ignored by default (125 total)**. Formatting, strict Clippy, optimized build and strict signing passed. The release, dist and independent QA preview executables shared UUID `5B058764-D3B9-330C-A643-EE9F32518F7C`.

Using the same release, independent QA opened the demo (1000 rows, 11 fields) through the system picker. Checks included four-sided single-cell outlines in both themes, dark rectangular drag selection, light row/column header selections and closed outlines across all four frozen regions. The bottom bar stayed at 22px and Memory kept updating from welcome through table display. All three mutually exclusive theme choices and the Follow System checkmark were verified. Selecting Follow System matched the system's current light appearance. Switching an open nested preview to dark then light via native Edit → Theme updated both its table and JSON input. Return to Cell worked. The original fixed light preference was restored without restarting the user's main process. QA left global macOS appearance untouched, so automatic system changes and high contrast had code/regression coverage without additional manual switching. The 5GB performance and full media suites were not rerun. See `system_theme_selection_memory` in [large-file-validation.json](large-file-validation.json). The 112/109 counts and UUIDs below belong to earlier builds.

## Earlier neutral color palette

The palette used IBM Carbon's neutral-gray layering and Radix Slate/Iris scales. Light mode used white and pale gray, dark mode graphite, and primary buttons, focus and selections used indigo. Three marking colors were adjusted for the new foreground, retaining visible fills and edge strips. Disabled buttons no longer applied an extra overall fade to their secondary text. See the [color system](color-system.md) for sources, adaptations and limits.

All **112 tests passed, with 0 failures and 1 audio hardware test ignored by default (113 total)**. Formatting, strict Clippy, optimized packaging and strict signature verification passed. The release/dist UUID for these automated checks and the palette GUI run was `4F0DEF03-9B31-36BC-B9A4-E1C4FA05C83A`. Semantic-color and marked-cell contrast regressions passed their existing thresholds.

After unlocking the session, checks on the delivered app covered:

- The dark welcome view and the demo's 1000 rows/11 fields, including header, alternating rows, text, NULLs and links to nested values.
- Fields in light and dark modes: search input at rest/focused, select all/none, a legible disabled Apply 0, the normal Apply 11 button and modal backdrop. Hover was checked only on the dark primary button.
- The filter sidebar in light and dark modes: input at rest/focused, selected All and the Not applied notice. Applying `id=0` in dark mode returned 1 result; clearing the draft and applying brought back 1000 rows.
- Blue/Teal/Amber row marks in light and dark modes, selected Amber retaining its hue, a Blue column fill and header strip, and ordinary column selection. Dark mode was restored.

The final patch synchronized all six existing import-wizard inputs with the theme. Final release/dist UUID `F21D8AFF-A939-3A9D-BB8C-6C88661D6746` again passed 112 tests with 0 failures and 1 ignored, plus formatting, strict Clippy, optimized packaging and strict signing. The restarted package showed the six fields in the dark import panel. In-place theme switching there did not receive a separate GUI regression. The full palette run belongs to `4F0DEF03-9B31-36BC-B9A4-E1C4FA05C83A`; the final patch added only import input synchronization. The large-file performance and full media suites were not repeated, and the old independent preview was left alone. Temporary `caffeinate` was stopped without changing persistent power settings. The final main window retained the user's current state.

## Earlier icon and image preview validation

This version added a geometric blue table logo on a white rounded tile, 39 Lucide command icons from a pinned source and 17px rounded checkboxes shared by field and import toggles. Double-clicking a Photo STRUCT or BLOB containing real image bytes prioritized a large image. Ordinary scalars and non-image BLOBs opened no popup; other nested containers still showed a table and JSON. Image and structure inspection used separate cache keys.

The suite passed **109 tests, with 0 failures and 1 audio hardware test ignored by default (110 total)**, plus formatting, strict Clippy, optimized packaging and strict signing. Release, dist and independent preview UUID: `446521B5-3393-3677-9C6E-F4D5A4D92545`.

Independent preview checks on the same release build covered:

- The new logo on welcome, the toolbar and About, and Lucide icons in the dark More menu.
- Selected and unselected rounded checkboxes in light/dark Fields. Deselecting one changed Apply 7 Fields to Apply 6 Fields. Select All allowed all seven columns to load.
- Double-clicking the Photo STRUCT and ordinary BLOB PNG in `target/demo/photo-ui.parquet` opened a proportional 16,516-byte PNG in the main preview area without a raw-data editor. Return to Cell worked.
- A normal details Struct still showed a table above formatted JSON. The original dark theme was restored before the preview ended.

The source PNG and ICNS were checked at 1024/32/16px and the ICNS unpacked into ten standard sizes. Automated regressions covered image priority, normal containers and scalar exceptions, and distinct preview-mode cache keys. Import toggles used the same checkbox but the round did not rerun the entire import workflow or 5GB performance tests. The user's original process was not restarted; the preview window was left open for inspection.

## Earlier title bar and nested preview

The 36px title bar incorporated the main command bar, and the preview shared window controls. Double-clicking a nested value opened roughly 55:45 table/JSON detail. Double-clicking an ordinary scalar no longer opened a preview. The 256 KiB JSON display budget applied when opening or navigating nodes. Full copy and save used the original content.

The suite passed **105 tests, with 0 failures and 1 audio hardware test ignored by default (106 total)**, plus formatting, strict Clippy, optimized build and strict signature verification. The release and binary inside the `dist` bundle shared UUID `E9E1A15B-E0F0-3AE0-B453-CA631FF84B7C`.

An independent preview app ran the final release executable and verified:

- The native picker opened the demo and a nested Struct List fixture. Double-clicking integers, decimals, timestamps, NULL, text or Boolean values created no new window. Single-click selected a nested cell and double-click opened it.
- The nested window showed a table above pretty JSON. Entering firstB showed id `1`; Back restored the parent; entering secondB showed id `null`. Node and JSON remained in sync, and Return to Cell took the user back to the source table.
- Title-bar More, Open and Reload responded. Reload completed after dragging the window, displaying 2ms in that run. Double-click zoom and entering/exiting fullscreen worked; native window buttons stayed hidden. This reload label was not a large-file performance measurement.

Minimize was clicked, but an automatic window-raising tool subsequently changed window state, so the visible outcome could not be verified. A debug build after the close-button fix confirmed exit. Closing the final release preview was verified to return to the main table. The complete window/input/media combinations and 5GB test were not rerun.

The macOS 27 title-bar event backport and upstream references, including the scope of AppKit custom title-bar event hooks, are in [GPUI PATCHES](../vendor/gpui/PATCHES.md). With an isolated application identity, the independent debug preview once remained at schema reading on the `__open` path. Final validation opened the source successfully through the native picker. The cause was not established; the observation does not prove a TCC issue. The delivered `dist` app also launched and selecting the recent demo led to Fields. Earlier tests and UUIDs retain their original scope below.

## Earlier highlighting, sidebar and resource configuration

The highlighting/menu cleanup build passed **102 tests with 0 failures and 1 ignored**, plus formatting and strict Clippy. The optimized app and release executable shared UUID `429CC005-9DA1-3782-AAAD-E8D45B24835C` and passed strict signing. An independent debug preview checked stronger fills, column-header and row-number strips and selected marks retaining their hue in light/dark modes. On restart, an independent copy of the final release showed the native Tools/Edit menus without table-creation scripts or Copy WHERE. Source inspection also confirmed removal of related cell/axis context commands, shortcuts and the metadata query-text page. No 5GB performance retest was run.

The stronger three-color fills retained their hues when selected; a 4px strip to the left of row numbers and a 3px strip beneath column headers improved recognition. Regression tests required at least 4.5:1 contrast for normal text, NULL/secondary text and nested links against both ordinary and selected marked backgrounds. Separate checks covered strips against fills. These checks had a narrower scope than full application accessibility. SQL-related UI commands and shortcuts were removed.

The preceding full suite passed **102 tests with 0 failures and 1 audio hardware test ignored by default (103 total)**. Formatting, strict Clippy, optimized build and `codesign --verify --deep --strict` passed. Bundle and release UUIDs were `B3014F44-B346-3B54-B388-E5AAB5B27631`.

That round's debug GUI verified the compact toolbar, single-line status, More menu and executing the Offset=500/Rows=10 dialog by Tab/Enter. The same round verified one row from id=2 AND active=true in the right-hand filter sidebar, retaining an unapplied draft across close/reopen, clearing filters and Load All. Dragging row numbers selected rows 3–5 for Amber marking and precise freezing while the normal area scrolled. Drag-selecting fare_amount/tip_amount, auto-width, Teal marking and horizontal scroll after freezing were checked. Interactions were sampled. The optimized package was not relaunched for another full GUI run.

The adaptive configuration build revalidated a 5,523,882,977-byte / 2,600,000-row Parquet file. Open took 24ms; first 1000 rows including a struct took 34ms; window filter/sort took 14ms; last 1000 including a struct took 16ms. Five exports and source-overwrite prevention passed; SHA-256/mtime were unchanged. The log is `target/large-data/threading-validation.log`, with artifacts isolated from past tests. Hashing warmed the file cache and no RSS/CPU saturation was collected for that run. It supports neither a controlled speedup nor a memory guarantee. GPUI's GPU rendering does not imply DuckDB runs queries on the GPU.

## Earlier nested table and text import

`cargo fmt --all`, `cargo clippy --locked --all-targets -- -D warnings`, the full suite and optimized packaging passed. **84 tests passed with 0 failures and 1 audio hardware test ignored by default (85 total)**. New regressions covered the nested model, read-only identity protection, delimited text and UTF-16 API boundaries.

GUI checks covered command icons, STRUCT/LIST tables, Back/copy, the import wizard, rules/type overrides, window sizing and failure recovery. Last id `2599999` in the 5.52GB Parquet file was fully visible. After restarting the release, the bottom bar in the 5.47GB `.records` wizard was directly operable. Setting `NA` as missing imported just 1000 rows; Chinese, quoted content, dates and booleans were correct, and the total remained unknown. See the [design document](nested-and-import.md) for actual resource and source-hash results for both large files.

The nested/import `.app` was about 45 MiB and passed `codesign --verify --deep --strict`. Bundle and release arm64 Mach-O UUID: `26CD9194-79B9-3969-A676-AF5D8D232819`. The package used local ad hoc signing, without Developer ID signing or Apple notarization. Validation covered Apple Silicon/macOS 27.2 and a subset of file/encoding/interaction combinations.

## Earlier UI palette

This version unified blue-gray/dark-blue themes, 6px control corners and the hierarchy of primary actions. Tables used alternating rows and subtle grid lines; a file information bar was added, and recent files/export formats used cards. Inputs kept a single border and 28px height. Import footer and option rows could wrap, and dialog heights were constrained to the window.

All 3 existing input regressions and 4 localization regressions passed, as did strict Clippy, optimized release and strict signing. Palette build bundle/release UUID: `3CCA8F2F-CB50-37E1-AC26-D9D8BFBA4FEE`. GUI checks covered both themes, Chinese filtering, fields, export, import and nested preview. The full suite and 5GB figures in the preceding section belong to the earlier feature build.

## Earlier logo display

The embedded logo was verified in the toolbar, welcome page and About. The launch callback set `NSApplication.applicationIconImage` on the main thread, including when running the binary directly. Cached Finder/Dock display was outside the scope of that check. The ICNS unpacked successfully and its native icon matched the in-window artwork. Packaging refreshed the completed `.app` directory timestamp.

Formatting, strict Clippy, optimized packaging, strict signing and matching bundle/release UUIDs passed for the logo fix, UUID `A0B909AE-5AE3-3F05-9B2F-B38D5A3CCA18`. This change did not rerun previous data or large-file performance regressions.

## Earlier visual filter dialog

The dialog supported multi-field All/Any, typed conditions, field search and local validation. The complete suite passed **92 tests with 0 failures and 1 ignored by default (93 total)**, including 8 new model and real-engine regressions. Formatting, strict Clippy, optimized packaging and signing passed.

The GUI tested id range 2–4 with active=true: AND returned 2 rows, OR returned 6, and removing the Boolean condition returned 3. Checks also covered Cancel, the then-current advanced-expression replacement prompt and Escape in the picker. Release/bundle UUID: `7D0F9E11-7C3C-3249-8102-8473B3E36A82`. The advanced entry point has since been removed, Close now retains sidebar drafts, and Escape hides the sidebar. Those earlier actions were validated only for that release.

## Validation before the redesign

The following tests, manual operations and package identities **predate the new icons, command bar, nested tables, text import and unified source protection**. They are retained for traceability, with separate validation for title bars and nested previews. The Arrow columnar/greedy section above describes the migration work; GUI results belong to the historical builds identified here. Separate large-file measurements are in the [design document](nested-and-import.md).

The earlier version passed `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, optimized release build and packaging. **56 tests passed and 1 audio hardware test was ignored by default (57 total)**; explicitly running the audio hardware test separately succeeded. Engine regressions covered real duplicate field names, dates, precision, nested values, nulls, snapshots, cancellation, WHERE sets and five export formats. Independent Calamine readback verified XLS/XLSX, including a workbook of about 17 MiB.

Actual GUI checks for that version included:

- Native Open/Save/Finder, initial field search, record windows, filtering/numeric sort and Chinese/null CSV. Language switching retained field names and data. Checks also covered themes, Metadata/Schema and DDL/Unicode clipboard contents.
- After dragging a column, names and values remained aligned (drag-to-reorder was subsequently removed). Right-click retained a six-cell selection. A WHERE using per-column IN/AND produced 3 rows.
- ⌘R preserved two fields, offset `200`, count `25`, filter and reordered columns. After a reload failure, CSV export from the old three-row snapshot was independently checked in Python.
- Three-line notes occupied about 70pt, null rows stayed short, and frozen id aligned vertically with other columns. A single drag to the bottom of a 5,000-row demo with wrap/freeze left row `4999` fully visible. Load All retained offset `200` and returned `4,800` rows.
- Native FRIDAY and MONDAY quick previews coexisted. Window listed the main window and both previews. Menu Next/Main, ⌘+backtick and ⇧⌘+backtick cycled them in order.
- Reopening the same FRIDAY cell kept only the main window and two previews, confirming reuse. After reloading the main window, Return from the old MONDAY preview kept it open and reported that the source/query had changed and the snapshot was old. Return from a new preview correctly located the original V1 value `1` and closed the preview.
- The new native photo preview showed a real PNG and the complete HEX value could be pasted after copying. The native nested preview followed `children(list) → 0(struct)` to `{ "value": 0 }`; the copied value pasted into a main-window input.
- Date Default/ISO/Custom, ⌘M metadata, ⌘N, inline WAV pause/seek and format information. The native audio preview displayed PCM/lpcm, 8,000 Hz, mono and 16-bit. Clicking its timeline reached `0.6 / 1.0 s`.
- The case-insensitive duplicate-name fixture showed a 14,610-row source. Both conflicting fields stayed gray and disabled after search while the other 10 fields remained available.

The old arm64 `.app` was about 44 MiB and passed `codesign --verify --deep --strict`. Its bundle and release binary had Mach-O UUID `C828E0D1-29E9-3C34-AED5-EEA0018CCA86`. It had neither Developer ID signing nor Apple notarization. The build and operations above were validated only on Apple Silicon/macOS 27.2.

These checks cover particular interactions. Back in the earlier nested preview was not tested separately. New nested tables, imports and other interactions require their own evidence. Intel/Universal builds, other macOS versions and manual checks of every image/audio encoding remained unverified. The .NET, locale, format and platform differences above still apply.

## Historical first/last navigation and Simplified Chinese

The historical package passed **176 tests with 0 failures and 1 audio hardware test ignored by default (177 total)** under `cargo test --locked`, plus formatting, strict Clippy, release packaging and strict deep signature checks for dist/QA. Final executable UUID: `1CFDE41F-5D8C-3731-8AF3-1B406C6F4121`.

GUI checks used a 1000-row window with nonzero offset 4096. First/Last landed on IDs 4096/5095 while selection, load range and query time stayed unchanged. The filtered 96 rows had endpoints 5000/5095, and navigation was disabled for an empty result. After freezing the first two rows, the ordinary pane still reached first/last. In a 1000-column table, First/Last from the rightmost column kept the horizontal position. Simplified Chinese menus, import, Fields, Filters, row menus, the status bar and language inheritance in new windows were verified. The Chinese preference was saved.

The main GUI run used `694B10AF-52B9-3B9B-A8DC-E0A881A00DBF`. The final package only adjusted static line breaks in Chinese help; a separate restart confirmed the saved language and complete help wrapping. The 5GB performance and full media GUI tests were not repeated; earlier records retain their original UUIDs.
