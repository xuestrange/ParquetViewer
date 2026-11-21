# Nested tables, columnar results, and read-only sources

The application uses Arrow and DataFusion. The [2026-11-01 migration report](datafusion-migration-2026-11-01.md) identifies the build UUID, performance comparisons, and final acceptance scope. The [earlier performance report](performance-2026-11-01.md) describes the methods and checks for Arrow loading and UI optimization build `967F582A-969E-3E1D-8700-4C90FB612730`. The older UUIDs, test counts, and measurements below refer to their respective builds.

This document covers the loading and memory model for Parquet and delimited text. See [data processing behavior and historical validation](data-behavior.md) for version-specific records. Implementation entry points are [source_loader.rs](../src/source_loader.rs), [csv_source.rs](../src/csv_source.rs), [columnar.rs](../src/columnar.rs), [engine.rs](../src/engine.rs), [export.rs](../src/export.rs), and [source_guard.rs](../src/source_guard.rs).
See [arrow_types.rs](../src/arrow_types.rs) and [parquet_compat.rs](../src/parquet_compat.rs) for type handling, and [nested.rs](../src/nested.rs) and [peek.rs](../src/peek.rs) for nested values and previews.

The Struct/List and same-window preview release `E2AEC0C9-2366-38AB-9A60-8921798F172E` passed 202 automated tests, 11 Struct/List queries against real files, and staged GUI checks. The README for that version has a section on Struct/List filtering and same-window preview. The final type-aware filtering and terminology release `B0CD9CDF-E6FC-393D-B61B-3034D01FBADB` passed 198 tests, 10 typed queries against real Parquet files, and staged GUI checks documented under type-aware filtering in that version's README. The build, 187 automated tests, 5.5 GB background run, and independent QA recheck for filters, source row numbers, and range information apply to `49202AF5`. The 184 automated tests, 5.5 GB run, and GUI checks for resident results and progress belong to the release described under full Parquet loading and progress validation in its README. The Parquet and Arrow format references below were checked on 2026-10-30. Consult the migration report for its implementation and validation scope.

## How nested values appear as tables

Parquet schemas and definition/repetition levels encode nested columns. Splitting delimited text cannot recover their logical collections and structures. The Arrow/Parquet reader interprets the source schema and produces typed Arrow `RecordBatch` values. The viewer retains containers, values, and validity buffers. The field labels use UI type names and do not imply an additional database conversion layer. Opening a structured cell explicitly builds its JSON/NestedTable representation in the background, without first converting the table to strings or JSON. Lists appear as JSON arrays and Struct/Map previews as JSON objects. The inspector retains type hints. Unsupported source conversions and export types return explicit errors. There is no second native compatibility snapshot. See [Parquet nested encoding](https://parquet.apache.org/docs/file-format/nestedencoding/), [Parquet logical types](https://parquet.apache.org/docs/file-format/types/logicaltypes/#nested-types), and the [Arrow columnar format](https://arrow.apache.org/docs/format/Columnar.html).

| Logical value | Table representation |
|---|---|
| STRUCT | One record, with child fields as columns in schema order when known |
| LIST/ARRAY of STRUCT | One row per element and one column per field, allowing absent fields and NULL records |
| Scalar or mixed LIST/ARRAY | One row per element in a `value` column, with navigation into child containers |
| MAP | `key` and `value` columns, with the key identified as a JSON string and its declared type shown |
| Empty collection, NULL, empty object | Each has its own state, distinct from an empty string |

Type labels preserve nested field order and recognize Struct, Map, List, `[]`, and fixed-length arrays. Commas inside quoted field names and DECIMAL parameters do not split columns. The preview distinguishes NULL records in lists, JSON `null`, and missing object fields. Use Back or the clickable path to navigate. Copying the current level produces TSV with column names and proper escaping for tabs, quotes, and newlines.

Each level shares one immutable tree through `Arc<serde_json::Value>` and stores only its path and column hints. Navigating into a child or back up avoids copying the whole nested table. Visible cells generate short summaries, including element or field counts for child containers. The preview uses GPUI's `uniform_list` to render visible rows with independent horizontal and vertical scrolling. The viewport bounds the number of UI elements. The original cell JSON still requires full decoding, initial column discovery may traverse the level, and copying the table generates the complete TSV. An arbitrarily large nested cell can therefore require substantial memory. See the pinned [uniform_list.rs](../vendor/gpui/src/elements/uniform_list.rs).

JSON is a preview representation. It cannot reconstruct original MAP key types, duplicate object keys, or every scalar's physical representation without loss. The inspector explains this limitation. Copying preview JSON/TSV and exporting the original Arrow snapshot are separate operations. When Map keys repeat or collide after formatting, JSON/Excel export uses an array of typed key/value entries to preserve them all. Parquet export retains the original Map type. Arrow's independent Struct, List, and null validity definitions support distinguishing structures, lists, and nulls. See the [Arrow Struct layout](https://arrow.apache.org/docs/format/Columnar.html#struct-layout).

## Partition schemas and logical type fidelity

Partition compatibility uses the actual Arrow schema. Field names and order must agree. The viewer recursively merges nullability for containers such as Struct/List/Map and checks extension names, extension metadata, and legacy INTERVAL semantics. It accepts only demonstrated lossless common DataTypes, including Null with a concrete type, compatible Binary storage, increased Time precision, and Float16 to Float32. Arrow performs casts. Overflow and incompatibility return errors instead of introducing NULL. The first partition supplies schema-level metadata and other partitions provide their own summaries. Arbitrary schema evolution and lossy conversions are outside this scope.

Parquet's canonical extensions recognize JSON and UUID logical annotations even without an embedded `ARROW:schema`. Field metadata supplies recursive type labels for top-level fields, Struct children, List elements, and Map keys and values. Export parses JSON into a tree and preserves exact integers without passing through Float. UUIDs display and export to JSON as canonical strings while retaining the underlying 16-byte values. The Parquet writer preserves the corresponding logical annotations. An ordinary VARCHAR or BLOB remains semantically distinct from JSON or UUID with similar labels or identical physical storage.

Legacy Parquet INTERVAL retains all three physical components in a logically marked `FixedSizeBinary(12)`. Selecting a single Arrow unit such as YearMonth or DayTime would discard other components. Display, preview, and JSON/Excel use the established canonical text, for example `1 month 2 days 00:00:00.003`, with a String in JSON. Original bytes stay in RAM and Parquet export restores the INTERVAL annotation. Only sorting builds Decimal128 comparison keys using one month = 30 days and one day = 24 hours. Keys are generated lazily for legacy INTERVAL leaves in the selected sort target. The original components remain available. Filtering, unsorted views, and sorting an unrelated field do not scan other INTERVAL columns for keys. Changing or clearing that sort target releases the old keys. Value filtering for JSON, UUID, and INTERVAL continues to support only NULL checks.

## All loaded values reside in system memory

The application reads selected fields and the source Offset/Rows range into an unfiltered, typed Arrow base. **All imported values reside in system memory.** Intersecting Parquet row groups decode in parallel into `RecordBatch` values collected in source order. Metadata lets the viewer skip nonintersecting partitions and row groups. The Parquet reader handles field projection and row selection.

Delimited text is parsed in batches with Arrow CSV. The first preview infers a schema from a prefix sample, by default the first 1,000 data records. Above the preview, users can choose the delimiter, toggle the header, change the inference sample size, and refresh. Common choices such as comma and tab appear first. Other printable ASCII characters are available, except the double quote reserved for quoting. Click the type under a field name to override it. The in-memory sparse byte index tracks complete records, so embedded newlines in quoted fields do not change record numbers. To reach an unindexed position for the first time, scanning resumes at the nearest indexed position. Subsequent jumps reuse recorded byte offsets. Reads parse values in the selected columns and leave the source read-only.

Delimited text supports UTF-8 with or without a BOM. Empty fields become NULL. Type inference preserves identifiers with leading zeros. If a value beyond the sample proves incompatible, users can change the type and retry. For files of 5 GB or more, users can select a row range and fields before deciding whether to load all records. Each window maintains its own text index.

**File → Reimport…** (⌘R) rereads the sample using the previous delimiter, header setting, and sample size. Manual type overrides are restored by field name. Click a type below the preview header to change it, then confirm to apply. Cancel returns to the previous table or loading step. If fields or their order have changed, or the new types conflict with applied filters, the window warns before confirmation that the table view will reset.

Confirming an import reuses the preview's schema and row-index starting point. Whole-file text filtering compacts the text buffers referenced by matches when doing so saves appreciable memory. Aggregation feeds existing Arrow arrays to DataFusion in batches and lets the query plan select fields.

Run `cargo run --locked --release --example check_csv_import -- --large` to verify this path. The example generates an approximately 5.48 GB CSV in a temporary directory and checks logical record positions, jumps before and after indexing, full loads of selected fields, filtering, aggregation, cancellation, and preview reuse. It verifies the source SHA-256 before removing the temporary directory. The reported memory is sampled RSS. The system page cache affects disk-read counts.

The window's DataFusion MemTable and its display/export snapshot share the base arrays through `ArrayRef`. With no filtering or sorting the viewer publishes the base directly. Otherwise DataFusion returns UInt64 base indices, and an `ArrowTable` view retrieves the corresponding values without copying a complete wide result. The base remains available so users can change or clear conditions. Filtering to one row does not trim the base to a single row. Older snapshots may retain the old base. Buffers are freed after the last related snapshot and window cache release them.

The old fixed 128 MiB cap per result was removed. Row and column virtualization limits UI elements to the viewport and its buffer. It avoids scanning all values for display text or NULL highlighting. The base, helper/result indices, and execution workspace still require RAM. UInt64 index values use about 8 bytes per row, and querying, concatenation, and sorting may require temporary memory.

Virtualization bounds the number of UI elements and the GPU drawing workload. Result array memory grows with loaded rows, selected fields, and value sizes. Full JSON or media previews, clipboard copies, exports, and query workspaces may allocate additional memory. Reduce the imported range or select fewer fields to reduce result memory. GPU viewport dimensions do not cap it.

Each window may use all `available_parallelism`. Parquet has a dedicated Rayon pool, while DataFusion uses the corresponding Tokio workers and target partition count. Windows do not share a common CPU or memory pool. Each Engine's DataFusion `GreedyMemoryPool` allowance equals worker count × 256 MiB, clamped to 2048–4096 MiB. Spill-capable operators use a private temporary workspace. Some operators cannot spill, and the base stays in RAM. Caches, indices, JSON, UI, media, and writers consume additional memory beyond the allowance. GPUI draws using the GPU. Arrow/Parquet and DataFusion execute on the CPU.

## Type-aware filter controls

Each field's declared Data type determines available operations, shortcuts, input syntax, and validation. The editor does not scan values or infer a different type because text looks like a number or date. The declared type appears beside the field name. Numeric controls offer comparison and inclusive ranges, text controls offer contains/prefix/suffix matching, and Boolean controls offer True/False. On schema changes the editor expands and caches Struct paths. Each child uses its own Data type. Primitive Lists support exact typed element membership/nonmembership, empty collection checks, and NULL checks. Complex Lists support empty collection and NULL checks. Map, Binary, and unknown types support only NULL checks.

Date, Timestamp, and Time have appropriate before/after/range labels. Date and Timestamp bounds can be chosen from a calendar. Changing a Timestamp date preserves any existing time, fractional seconds, and explicit time zone. Equality compares the entire value without expanding to a day. An empty Timestamp starts at midnight and a zoned type starts explicitly at `+00:00`. Time controls show the appropriate time and time-zone format. Typed literals are checked for type, valid date, range, and precision. Numeric values are never first converted to Float to build conditions.

Simplified Chinese localizes ordinary actions and hints. The terminology convention in the recorded version retained terms such as Data type, Boolean, Date, Timestamp, Struct, List, Map, Schema, Metadata, Binary, Decimal, Float, Double, and True/False in English. Field names, values, and JSON keep their source text. Build `B0CD9CDF` passed strict Clippy, deep signature verification, and 198 tests. Actual typed Parquet query results are in `target/typed-filter-validation.json`. The first GUI pass on build prefix `5855` checked numeric, Date, Boolean, VARCHAR, and Time controls. After repairs, `B0CD9CDF` checked Timestamp_NS with surrounding whitespace and the calendar's initial date. Selecting a new date preserved nine fractional digits and matched `id=28`/source row `29`. The test also checked the single-line calendar format hint and that version's NULL-only List operators. The historical README gives the scope of those checks. The migration has separate validation. The RAM, resource allocation, and source-row policies below describe the implementation documented here.

## Struct paths and List element filters

The field picker reads existing schema labels and exposes Struct children at each level as separate targets. A condition stores its root field and `Vec<String>` path segments independently of its display label. Literal periods or quotes in actual names do not split paths. DataFusion expression generation handles each identifier and each Struct key individually. The editor caches the target type, operators, display labels, and search text. Typing or repainting does not traverse data columns or expand nested types again.

Primitive List membership compares exact values of the original type. Boolean, date, and time elements use their respective validation and inputs. Number preserves Decimal and large-integer precision. Text elements match the complete, case-sensitive value, while ordinary Text contains matching ignores case. Empty, nonempty, NULL, and non-NULL Lists are distinct. Lists of Struct, Map, List, or other complex elements currently support empty collection and NULL checks.

## Same-window preview lifecycle

Nested values, images, and explicit Inspect actions open a preview on the right of the owning main window. Each data window has at most one preview. Both preview and filter sidebars use 360 logical pixels, a single-line title with Close, and consistent padding, spacing, and button styles. Narrow type summaries truncate visually and supply a full tooltip. The nested table and JSON occupy roughly 55:45 of the content area. Buttons wrap in narrow panes and images preserve their aspect ratio.

Filters and preview are mutually exclusive. Opening filters closes preview and preserves the filter draft. Close or Esc releases the preview entity and its references to media and Arrow slices. The main table continues to own its loaded data. Replacing the actual query snapshot or source closes an old preview. A repeated no-op Apply retains it. Preview titles include the original row number, and Close in the upper-right collapses the pane. Copy Node, Copy Cell, Save Value, image copying/saving, and nested navigation remain available.

The final Struct/List build UUID was `E2AEC0C9-2366-38AB-9A60-8921798F172E`. It passed 202 automated tests, formatting, strict all-target Clippy, release build, and deep signature verification. The 11 real Parquet queries are in `target/nested-filter-validation.json`. Unit regressions also cover 25 combinations of NULL, empty collections, precision, and special field names. Initial build `975C37EA` checked an exact decimal Struct child and combined List filters. `EFBA` provided an intermediate GUI check for the 360px pane and Close control. After the type summary changed, final build `E2AEC0C9` separately checked the Struct table/JSON, a List containing 28 and source row 28, empty/nonempty labels, draft retention across panels, Copy Cell by pasting, Photo SVG, Esc closing only the pane, and correct results after reload discarded the old preview. The historical README records the steps and version assignments. That pass did not rerun the full 5 GB, all-theme, or media suites, and the user's original application remained running. Earlier independent Quick Peek window, copy, theme, and return-position checks belong to their respective historical builds.

## Reusing an imported window and preserving source row numbers

Parquet metadata stores partition row counts and cumulative offsets. A request determines its source range, skips partitions and row groups outside it, and reads the selected fields. In the loaded-window path described here, filtering and sorting operate on the imported Arrow base. Value predicates do not push down into the Parquet decoder on this path. The source window is established before filtering/sorting. Initial import still reads the selected values in that window. Reusing a cached window does not make every condition scan-free.

Changing filters or sorting with the same source, selected fields, and Offset/Rows reuses the existing base. Compatible query shapes also reuse the DataFusion context. A change of legacy INTERVAL sort target rebuilds the context to replace comparison keys without rereading the base. The MemTable's base-index helper and the result index are independent of data columns. When DataFusion sorts globally across batches, only indices are returned to the result snapshot. Wide table value buffers stay in their original order. Text sorting ignores case, places NULL last, and uses the base index as a stable tiebreaker. Map and nested Map sort keys temporarily use lists of key/value structs sharing their buffers. The original Map remains available for filtering, preview, and export. A legacy INTERVAL in the key first receives the comparison key described above.

Source row identity is independent of query ordering. It starts at `1` for a single file, and for multiple partitions it includes the cumulative offsets in sorted discovery order. Filtering can leave gaps. Sorting changes display position while preserving source identity. Main-table row numbers, preview titles, and the cell inspector can refer to that identity. Selection, freezing, and scroll positioning use current result positions. The helper identifier is separate from user fields. It neither appears as an extra column in clipboard or JSON/XLS/XLSX/Parquet output nor hides a user field of the same name.

If the source appears unchanged and the new request exactly matches the displayed request, repeating Apply skips execution. Reloading Parquet reopens the source, reads its latest schema, and validates and rebuilds applied conditions against it. The new Engine and snapshot replace the old state together only after both succeed. Missing fields, invalid types or conditions, cancellation, or failure leave the old result usable. The old schema is never used to reinterpret new data. Changing fields or row range also rebuilds the base. Existing immutable snapshots can still be exported independently. A sidebar repaint neither copies the schema nor compiles filters. Loaded Arrow values remain in system memory, with per-window CPU use and no cross-window resource budget.

## Progress follows the execution path

Loading, filtering, and statistical analysis show task status at the bottom of the window while active. Aggregation reports rows read and then identifies the final merge and charting phases. The active job can be cancelled from the status bar.

During Parquet reads, the percentage is decoded rows divided by the requested window row count from metadata. No extra scan is needed for the denominator. Worker counters merge monotonically. They count rows loaded into the base, including rows a later filter may exclude. DataFusion then reports filtering or sorting. When operator completion cannot be estimated, the UI shows the phase and the number of indices emitted, without treating output as scanned rows or inventing an overall percentage. Editing conditions within the same window reuses the completed read phase. Without a filter or sort, the viewer publishes the shared base directly and the preparation phase avoids building a second complete result. Cancellation and failure preserve the displayed snapshot.

Processing uses Arrow `RecordBatch`, shared `ArrayRef`, schema, projection, cast, concat, and interleave. DataFusion handles filtering and sorting without a separate row-by-row Rust engine. Explicit previews and format-specific exports still require value formatting. Export reads an immutable snapshot directly. It does not register a second database reader, reread the source, or persist derived Arrow IPC.

Export advances in batches of at most 8192 rows. Contiguous base data uses shared slices. Indexed views use Arrow interleave to materialize the small segment in displayed order, then project selected fields. Parquet uses ArrowWriter, and JSON is written one cell at a time. XLSX uses a constant-memory worksheet and temporary storage. XLS retains its SST/BIFF format caches. A row cap per batch does not impose the same byte limit on writers or individual large values. Maps with ordinary unique keys become JSON objects. If keys repeat or collide after formatting, JSON/Excel uses an array of entries such as `[{"key": <original typed key>, "value": <value>}, …]` to retain every entry. This also applies recursively to nested Maps. Parquet preserves the original Map container and values, and exports omit sort helpers.

The unwrapped main table uses fixed 28px rows and constant-size scroll metadata. Initializing, resizing, or toggling a sidebar avoids building a layout entry per data row. Only visible rows create elements. Enabling word wrap switches to a variable-height list with per-row metadata. Auto-width samples at most 256 current result rows. Releasing old results and disconnecting the Engine run in the background so bulk buffer deallocation does not block the main thread.

In the layout recorded here, **Load Range…** and **Load All** were directly on the toolbar. Load Range sets Offset/Rows, defaulting to `0`/`1000`, and displays total source rows and rows remaining from the current offset. It uses existing metadata and updates the remaining count as the offset changes. Load All resets the offset to zero and expands Rows. Filtering in this workflow applies to the requested window. First/Last scrolls within the current result without rereading the start or end of the source.

## Read-only source guarantees

The application provides no in-place cell editing. Parquet sources open through read-only file interfaces. DataFusion applies sidebar-generated conditions and sorting to the imported Arrow base. There is no advanced SQL input or hidden legacy query compatibility API. Read-only source protection also uses independent output-path guards.

All application outputs use a session-wide source guard. It records the original absolute path, resolved path, and device/inode identity to block overwriting an opened source or its aliases through symbolic links, hard links, and directory symlinks. A source remains registered for the process session after its window or preview closes. Even deletion leaves its path reserved. Data export, media/cell Save As, reports, and preferences share this protection. A source named `preferences.json` cannot be overwritten by saving settings.

Outputs are written first to a temporary file in the destination directory. Successful, uncancelled writes recheck the target before committing with rename. `source_guard::save_copy` uses `create_new`, avoids following pre-existing temporary symlinks, and leaves unrelated temporary objects alone. The four data exports documented in this version share `export_snapshot`, which opens a same-directory temporary file with `create_new` and hands it to the relevant writer. Temporary names include a time-based nonce, process ID, and atomic sequence number. Source protection and cancellation are checked before writing and committing. Export uses the displayed snapshot with its fields and sorting, and a failed export does not truncate the destination to a partial file.

These guarantees apply to reads, queries, and output paths managed by the application. The app still writes requested derived copies, preferences, and temporary data. It does not change source file permissions or prevent other processes from modifying files. Path and inode checks provide application-level protection without offering OS-level isolation against malicious concurrent filesystem replacement. Regression tests cover ordinary aliases, closed sources, old previews, preference-path collisions, pre-existing temporary files/links, and cleanup after failed commits. See the tests in `source_guard.rs`.

One historical intermittent temporary-export failure had an unconfirmed root cause. After an atomic sequence number was added, its targeted regression and 20 additional concurrent reruns passed. Those runs neither established the root cause nor retested every current writer.

## Scope of the historical validation records

The old query engine, text import, CSV export, additional loading entry points, and resident-result budgets in the following records belonged to earlier versions. Some were subsequently removed or replaced. The original UUIDs, counts, paths, measurements, and scope are retained for traceability. Consult the migration report for validation of the Arrow + DataFusion architecture.

## Historical random paging of text files (removed implementation)

The historical package returned **169 passed, 0 failed, 1 audio-hardware test ignored by default, 170 total** from `cargo test --locked`. Formatting checks, strict Clippy (`--locked --all-targets -- -D warnings`), optimized packaging, and strict deep signature verification of the dist and independent QA copies passed. The release, dist, and QA UUIDs all matched `E262D7C4-5243-33B8-8F61-763B9BFE758B`.

Independent QA of that final release covered indexed text paging only. Native Open loaded `target/demo/text-index-qa-1793447584008068000.records` (406,974 bytes, 9007 rows, 4 fields). The wizard sampled 1000 records and showed the pipe delimiter and header, Chinese text, quoted newlines, and a decimal NULL. The initial window showed 1000 rows starting at id0, an unknown total, and a 6ms UI timing. Entering offset9000/Rows1000 under More → Load Range returned 7 rows, id9000–9006, NULL at id9002, an exact total of 9007, and 10ms. At offset4096 it showed 1000 rows and retained the total of 9007. Returning to offset0 restored the 1000 rows starting at id0. Both jumps showed 8ms. These figures are GUI status and timing labels for a small fixture, separate from 5GB GUI tests or controlled benchmarks.

Independent QA exited with ⌘Q without opening or restarting the original app, changing system settings, or using caffeinate. This package did not repeat the earlier wide-table/NULL/image/full-media GUI suite. A separate standalone check covered 12,553 rows in each of UTF-8, UTF-16LE, and UTF-16BE. For each encoding, 60 random and boundary jumps matched a complete CSV decode value by value, including Unicode and mixed newlines. This was separate from the Cargo test count.

A local release example passed a single regression against a real pipe-delimited `.records` file of **5,468,522,249 bytes / 2,600,000 rows**. Prefix inference took 59ms and opening took 26ms. The report is in `target/text-random-pages-1793447530360820000/report.json` and the resource log in `target/text-random-resources-1793447530360823000.json`.

| Request | Offset / rows | Elapsed | Record-framing bytes for request |
|---|---|---:|---:|
| First 1000 rows | 0 / 1000 | 24ms | 2,096,469 |
| First unindexed final page | 2,599,000 / 1001, returned 1000 | 4658ms | 5,466,425,780 |
| Indexed random page 1 | 1,300,000 / 1000 | 26ms | 5,403,252 |
| Indexed random page 2 | 17 / 1000 | 23ms | 2,132,131 |
| Indexed random page 3 | 2,599,000 / 1000 | 26ms | 6,598,368 |
| Indexed random page 4 | 500,000 / 1000 | 23ms | 2,707,652 |
| Indexed random page 5 | 2,000,000 / 1000 | 22ms | 4,527,948 |
| Indexed random page 6 | 1,300,000 / 1000 | 23ms | 5,403,252 |

The on-disk index grew from 32 bytes after the first window to **20,320 bytes** after the first final-page seek. EOF confirmed a total of 2,600,000 and an empty page beyond EOF needed no further scan. “First unindexed” refers only to the index state, **not a cold operating-system cache**. Framed bytes count the logical record-framing span and exclude other process I/O and scanner read-ahead.

Checks covered values on every page, quoting and NULL rules, exporting an old snapshot after random jumps, and blocking source overwrites. Source SHA-256 was `81d8c96f253c6a1eb186a7e89c60b70c263b5208f7906f7751faa52262636dc3`. Device/inode, size, mtime, and ctime all matched before and after. The test program's `wait4` peak RSS was **49,348,608 bytes (47.0625 MiB)**. Total elapsed time was approximately **37.77s**, including two full source SHA-256 passes. These values do not measure query time or GUI memory. The run was not a controlled before/after comparison with the old unindexed debug result of 51.698s, so no speedup ratio follows. It measured no FPS, p95, or CPU profile, and establishes no RSS improvement.

See the structured [text_random_paging record](large-file-validation.json). The earlier unindexed results below retain their own timings.

## Historical large-file validation without a text index

This was one local automated range-loading run of an early version without a text index. The two validation processes ran concurrently and competed for CPU and disk during processing and SHA-256 calculation. The figures measure neither exclusive-machine performance nor GUI frame rate. Original reports and `/usr/bin/time -l` resource logs are in `target/large-data/{validation,text-validation}.json` and `target/large-data/{parquet,text}-resources.log`. See [check_large_fixture.rs](../examples/check_large_fixture.rs) and [check_large_text.rs](../target/migration-baseline-20261101/examples/check_large_text.rs).

| Measurement | Parquet observation |
|---|---:|
| Actual file size | 5,523,882,977 bytes |
| Source rows | 2,600,000 |
| Open schema/metadata | 26 ms |
| First 1000 rows, including nested STRUCT | 53 ms |
| Filter and sort loaded window | 14 ms |
| Last 1000 rows, including nested STRUCT | 18 ms |
| Validation process peak RSS | 170,721,280 bytes (approximately 162.8 MiB) |
| Validation process swaps | 0 |

The run read the first and last ranges, checked Chinese text and nested list/struct values, filtered and sorted, exported derived CSV, JSON, Parquet, XLS, and XLSX results, and verified that none of the five formats could overwrite the source. Source mtime was unchanged and both SHA-256 passes yielded `068e250e905273d0a52dfeab6815febdb1aaba111beda7030b0f914f643a130a`.

The text run used an actual pipe-delimited file with the `.records` extension. Opening it did not scan every record to establish the count.

| Measurement | Delimited text observation |
|---|---:|
| Actual file size | 5,468,522,249 bytes |
| Actual source data records | 2,600,000 |
| Prefix inference | 69 ms |
| Bytes in the selected prefix sample | 2,094,369 bytes |
| Open configured source | 29 ms |
| First 1000 data records | 43 ms |
| Last 1000 records at offset 2,599,000, sequential prefix skip | 7,695 ms |
| Validation process peak RSS | 544,440,320 bytes (approximately 519.2 MiB) |
| Validation process swaps | 0 |

The text report recorded an initially unknown total. Requesting `1001` rows and receiving only the final `1000` at EOF established the exact 2,600,000 total. Window filtering/sorting, derived CSV export, blocked source-path overwrite, and before/after hash and mtime checks passed. Both hashes were `81d8c96f253c6a1eb186a7e89c60b70c263b5208f7906f7751faa52262636dc3` and mtime was unchanged. This text test exported CSV only. The Parquet test above covered export and source protection for each of the five formats. The sample byte count measures the logical record prefix for the selected delimiter. Inference examined prefixes separately for candidate delimiters, so the sample count does not represent total process I/O.

Both programs hashed the entire source before timing any queries, warming the system file cache. The results do not measure cold disk throughput or guarantee performance for other 5GB files. Process peak RSS covers the entire respective validation run and cannot be attributed independently to each query. The historical runs used two threads and a 512MB DuckDB managed budget. The text process illustrates that the budget was not a hard process RSS cap. A separate regression for the later adaptive-thread build is recorded in `target/large-data/threading-validation.log`. It does not support a controlled speed comparison. This test measured no memory bound for loading all 2.6 million rows with all nested values, and the newer index makes no such guarantee.

## GUI validation of the nested/import debug build

The following observations are from the Apple Silicon debug application for the nested/import release. Later release validation appears in the historical sections of `data-behavior.md`, which separately records the compact layout, filter sidebar, and table-state checks. This run did not manually cover every icon, encoding, and interaction combination.

- **Command icons.** Open, Folder, Import, Reload, Fields, Metadata, Export, Read-only Source, Execute, Clear, Load All, and the buttons in the tested dialogs rendered clearly in the actual window. Asset parsing and dimension checks separately covered all 38 icons.
- **Nested demo table.** Clicking a main-table STRUCT opened one row and two columns. Entering `children` displayed a table of records, and Back returned correctly. Copy Table pasted successfully and included column names such as `name`, `children`, and `value` from the current level.
- **Text inference.** `target/demo/import-check.records` was 45,144 bytes (about 44.1 KiB) and contained 1,005 data records, pipe delimiters, CRLF, quoted newlines, and Chinese text. The wizard reported sampling the first 1,000 logical records. Inferred types included BIGINT, VARCHAR, DECIMAL, BOOLEAN, and DATE. The `amount` sample showed 143 missing values and the corresponding NA/NULL rules.
- **Ranges and recovery.** Entering count `5` showed only 5 records and the footer total remained unknown. A range containing later mixed values produced a type error and retained the old five records. Requesting 1,005 through Import settings produced the same error while preserving both wizard and old table. Changing `amount` to VARCHAR and entering count `1006` reached EOF with 1,005 rows and an exact total. ⌘End showed id `1003` with `mixed later value` and the final id `1004`.
- **Source integrity.** An independent Python check found matching before/after SHA-256 and `mtime_ns`. The SHA-256 was `6e73967c619b39d53c9b5e99646f7f9e3176fa595a2cf34291bbe8905391db63`. See `target/demo/import-check-before.json`.
- **Real 5.52GB Parquet window.** Opening the 2,600,000-row source in the GUI loaded only 1,000 rows. Clicking the four-field STRUCT and entering `measurements` showed two rows, two columns, and a DECIMAL type. The `customer` subtable showed Chinese text and Back worked. Loading the final 1,000 rows at offset `2599000` and pressing ⌘End showed final id `2599999` in full.

These GUI steps verified table presentation, hierarchical navigation, import rules, range selection, and error recovery. The earlier timing and RSS figures came from separate automated processes and do not measure this GUI session.

The final full regression suite reported **84 passed, 0 failed, 1 ignored by default, 85 total**. `cargo fmt --all`, strict Clippy, optimized release packaging, and strict signature checks passed. The suite included the UTF-16 advanced API regression. Normal LE/BE offset `1` and limit `1` returned id `2`. Unsupported nonempty advanced SQL was rejected before transcoding. It created no derived file, kept the old snapshot, allowed the subsequent ordinary filter to run correctly, and left source bytes unchanged.

The final release was restarted and completed a wizard import of the 5.47GB `.records` file. The fixed footer controls were usable without scrolling. After specifying `NA` as a missing marker, the table showed 1000 rows with the expected Chinese text, Boolean and date values, and NULLs. The source total remained unknown. The optimized arm64 package was about 45 MiB, passed strict signature verification, and had Mach-O UUID `26CD9194-79B9-3969-A676-AF5D8D232819`. It was signed locally ad hoc without Developer ID signing or Apple notarization.

## Reproducing the large-file Parquet range test

The generator refuses to overwrite an existing file. The source and logs live under `target/large-data`. The fixture physically writes every record without using a sparse file or padding the end. These programs mainly check range reads, nested values, derived outputs, and source protection. Full-load memory, progress, and GUI behavior require separate validation.

```sh
# Generate the source only if the approximately 5.52GB fixture is absent.
cargo run --locked --release --example make_large_fixture
cargo run --locked --release --example check_large_fixture
```
