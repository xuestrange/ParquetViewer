# Nested tables, columnar results, and read-only sources

This document covers the loading and memory model for Parquet and delimited text. See [data behavior](data-behavior.md) for filtering and table interactions. Implementation entry points are [source_loader.rs](../src/source_loader.rs), [csv_source.rs](../src/csv_source.rs), [columnar.rs](../src/columnar.rs), [engine.rs](../src/engine.rs), [export.rs](../src/export.rs), and [source_guard.rs](../src/source_guard.rs).
See [arrow_types.rs](../src/arrow_types.rs) and [parquet_compat.rs](../src/parquet_compat.rs) for type handling, and [nested.rs](../src/nested.rs) and [peek.rs](../src/peek.rs) for nested values and previews.

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

## Browsing and loading rows

When you load a range, the application reads its selected fields into a typed Arrow base in memory. Intersecting Parquet row groups decode in parallel into `RecordBatch` values collected in source order. Metadata lets the viewer skip nonintersecting partitions and row groups. The Parquet reader handles field projection and row selection.

Opening a delimited text file samples up to 1,000 rows or 256 KiB, finishing the current record, and displays the first typed page. The worker continues from that sample and writes sparse row and byte offsets to a temporary file. Quoted newlines stay inside the same record. The footer shows how many rows are available and the percentage of bytes indexed. The exact total appears at the end of the scan.

The background scanner counts fields and finds record boundaries with fixed-size buffers, including when a field spans multiple reads. It uses vectorized byte searches on supported CPUs to count separators in unquoted rows and skip long stretches of ordinary field data. The CSV parser handles quoting and records split across buffers. The scanner checks each record against the sampled field count and the 64 MiB record limit. The index stores a 16-byte checkpoint after 16,384 records or 1 MiB of input at the next record boundary. The worker publishes the first batch promptly and groups subsequent writes. Typed page decoding uses its own working memory.

Scroll through indexed rows or use **File → Go to Row…** with a one-based source row number. You can keep browsing while a jump waits for indexing. Browsing uses fixed-height rows and a byte-limited cache of decoded pages. For word wrap, analysis, or export, load a fixed range. To return to the indexed table, use **File → Browse Text File**. **Load All Records** explicitly loads all selected data into memory.

**File → Import Text File…** opens the preview for any extension. It samples the first 1,000 data records by default. Above the preview, users can choose the delimiter, toggle the header, change the inference sample size, and refresh. Printable ASCII delimiters are supported, apart from the double quote reserved for quoting. Click the type under a field name to override it. Delimited text supports UTF-8 with or without a BOM. Empty fields become NULL. Type inference preserves identifiers with leading zeros. If a value beyond the sample proves incompatible, users can change the type and retry. Each window maintains its own text index.

**File → Reimport…** (⌘R) rereads the sample using the previous delimiter, header setting, and sample size. Manual type overrides are restored by field name. Click a type below the preview header to change it, then confirm to apply. Cancel returns to the previous table or loading step. If fields or their order have changed, or the new types conflict with applied filters, the window warns before confirmation that the table view will reset.

Confirming an import reuses the preview's schema and row-index starting point. Whole-file text filtering compacts the text buffers referenced by matches when doing so saves appreciable memory. Aggregation feeds existing Arrow arrays to DataFusion in batches and lets the query plan select fields.

Run `cargo run --locked --release --example check_csv_import -- --large` to verify this path. The example generates an approximately 5.48 GB CSV in a temporary directory and checks logical record positions, jumps before and after indexing, full loads of selected fields, filtering, aggregation, cancellation, and preview reuse. It verifies the source SHA-256 before removing the temporary directory. The reported memory is sampled RSS. The system page cache affects disk-read counts.

To check browsing with a file larger than RAM, run `cargo run --locked --release --example check_csv_import -- --browse-gib 64` on a volume that supports sparse files. This creates a temporary 64 GiB logical CSV with quoted multiline records, checks typed rows during indexing and at EOF, and reports the time to the first decoded page and the command-line process's sampled peak RSS.

To measure background indexing alone, build `cargo build --locked --release --example check_csv_import` and run `python3 examples/benchmark_csv_index.py current=target/release/examples/check_csv_import`. To compare builds, pass both `before=/path/to/before` and `after=/path/to/after`. The driver writes temporary CSVs, warms the filesystem cache, and runs each trial in a fresh process. The JSON report includes indexing time, process peak RSS, and separate checks for first-page and random-jump latency.

For a loaded range, the query engine and its display/export snapshot share the base Arrow arrays. An unfiltered, unsorted view can use the base directly. A filtered or sorted view can refer to rows by index while keeping the wide values in their original buffers. Older snapshots may retain those buffers until their last owner releases them.

Row and column virtualization limits UI elements to the viewport and its buffer. It avoids scanning all values for display text or NULL highlighting. The base, row indices, and execution workspace still require RAM. Row indices use UInt32 or UInt64 according to the size of the base, and contiguous positions can be stored as ranges.

Virtualization bounds the number of UI elements and the GPU drawing workload. Result array memory grows with loaded rows, selected fields, and value sizes. Full JSON or media previews, clipboard copies, exports, and query workspaces may allocate additional memory. Reduce the imported range or select fewer fields to reduce result memory. GPU viewport dimensions do not cap it.

Each window may use all `available_parallelism`. Parquet has a dedicated Rayon pool, while DataFusion uses the corresponding Tokio workers and target partition count. Windows do not share a common CPU or memory pool. Each Engine's DataFusion `GreedyMemoryPool` allowance equals worker count × 256 MiB, clamped to 2048–4096 MiB. Spill-capable operators use a private temporary workspace. Some operators cannot spill, and the base stays in RAM. Caches, indices, JSON, UI, media, and writers consume additional memory beyond the allowance. GPUI draws using the GPU. Arrow/Parquet and DataFusion execute on the CPU.

## Type-aware filter controls

Each field's declared Data type determines available operations, shortcuts, input syntax, and validation. The editor does not scan values or infer a different type because text looks like a number or date. The declared type appears beside the field name. Numeric controls offer comparison and inclusive ranges, text controls offer contains/prefix/suffix matching, and Boolean controls offer True/False. On schema changes the editor expands and caches Struct paths. Each child uses its own Data type. Primitive Lists support exact typed element membership/nonmembership, empty collection checks, and NULL checks. Complex Lists support empty collection and NULL checks. Map, Binary, and unknown types support only NULL checks.

Date, Timestamp, and Time have appropriate before/after/range labels. Date and Timestamp bounds can be chosen from a calendar. Changing a Timestamp date preserves any existing time, fractional seconds, and explicit time zone. Equality compares the entire value without expanding to a day. An empty Timestamp starts at midnight and a zoned type starts explicitly at `+00:00`. Time controls show the appropriate time and time-zone format. Typed literals are checked for type, valid date, range, and precision. Numeric values are never first converted to Float to build conditions.

## Struct paths and List element filters

The field picker reads existing schema labels and exposes Struct children at each level as separate targets. A condition stores its root field and `Vec<String>` path segments independently of its display label. Literal periods or quotes in actual names do not split paths. DataFusion expression generation handles each identifier and each Struct key individually. The editor caches the target type, operators, display labels, and search text. Typing or repainting does not traverse data columns or expand nested types again.

Primitive List membership compares exact values of the original type. Boolean, date, and time elements use their respective validation and inputs. Number preserves Decimal and large-integer precision. Text elements match the complete, case-sensitive value, while ordinary Text contains matching ignores case. Empty, nonempty, NULL, and non-NULL Lists are distinct. Lists of Struct, Map, List, or other complex elements currently support empty collection and NULL checks.

## Same-window preview lifecycle

Nested values, images, and explicit Inspect actions open a preview on the right of the owning main window. Each data window has at most one preview. Narrow type summaries truncate visually and supply a full tooltip. The nested table and JSON occupy roughly 55:45 of the content area. Buttons wrap in narrow panes and images preserve their aspect ratio.

Filters and preview are mutually exclusive. Opening filters closes preview and preserves the filter draft. Close or Esc releases the preview entity and its references to media and Arrow slices. The main table continues to own its loaded data. Replacing the actual query snapshot or source closes an old preview. A repeated no-op Apply retains it. Preview titles include the original row number, and Close in the upper-right collapses the pane. Copy Node, Copy Cell, Save Value, image copying/saving, and nested navigation remain available.

## Reusing an imported window and preserving source row numbers

Parquet metadata stores partition row counts and cumulative offsets. A request determines its source range, skips partitions and row groups outside it, and reads the selected fields. In the loaded-window path described here, filtering and sorting operate on the imported Arrow base. Value predicates do not push down into the Parquet decoder on this path. The source window is established before filtering/sorting. Initial import still reads the selected values in that window. Reusing a cached window does not make every condition scan-free.

Changing filters or sorting within the same loaded range can reuse the existing base. Sorting streams the Arrow batches to DataFusion and returns row indices. Text sorting ignores case and the base index breaks ties in source order. Map and nested Map sort keys use lists of key/value structs sharing their buffers. The original Map remains available for filtering, preview, and export. A legacy INTERVAL in the sort target receives the comparison key described above.

Source row identity is independent of query ordering. It starts at `1` for a single file, and for multiple partitions it includes the cumulative offsets in sorted discovery order. Filtering can leave gaps. Sorting changes display position while preserving source identity. Main-table row numbers, preview titles, and the cell inspector can refer to that identity. Selection, freezing, and scroll positioning use current result positions. The helper identifier is separate from user fields. Clipboard and exported data contain only user fields.

If the source appears unchanged and the new request exactly matches the displayed request, repeating Apply skips execution. Reloading Parquet reopens the source, reads its latest schema, and validates and rebuilds applied conditions against it. The new Engine and snapshot replace the old state together only after both succeed. Missing fields, invalid types or conditions, cancellation, or failure leave the old result usable. The old schema is never used to reinterpret new data. Changing the row range reloads the base. Within a cached range, a field change can reuse available columns and load missing ones. Existing immutable snapshots can still be exported independently. A sidebar repaint neither copies the schema nor compiles filters. Loaded Arrow values remain in system memory, with per-window CPU use and no cross-window resource budget.

## Progress follows the execution path

Loading, filtering, and statistical analysis show task status at the bottom of the window while active. Aggregation reports rows read and then identifies the final merge and charting phases. The active job can be cancelled from the status bar.

During Parquet range reads, progress counts decoded rows against the requested range using the metadata row count. The count includes rows a later filter may exclude. Filtering a loaded range without sorting reports rows processed from that range. Sorting reports the number of indices emitted while the total is unknown. Cancellation and failure preserve the displayed snapshot.

Processing uses Arrow `RecordBatch`, shared `ArrayRef`, schema, projection, cast, concat, and interleave. DataFusion handles filtering and sorting without a separate row-by-row Rust engine. Explicit previews and format-specific exports still require value formatting. Export reads an immutable snapshot directly. It does not register a second database reader, reread the source, or persist derived Arrow IPC.

Export projects the selected fields first and advances in batches of at most 8192 rows. Contiguous base data uses shared slices. Indexed views gather the selected values in displayed order. Parquet uses ArrowWriter, and JSON is written one cell at a time. XLSX uses a constant-memory worksheet and temporary storage. XLS retains its SST/BIFF format caches. A row cap per batch does not impose the same byte limit on writers or individual large values. Maps with ordinary unique keys become JSON objects. If keys repeat or collide after formatting, JSON/Excel uses an array of entries such as `[{"key": <original typed key>, "value": <value>}, …]` to retain every entry. This also applies recursively to nested Maps. Parquet preserves the original Map container and values, and exports omit sort helpers.

The unwrapped main table uses fixed 28px rows and constant-size scroll metadata. Initializing, resizing, or toggling a sidebar avoids building a layout entry per data row. Only visible rows create elements. Enabling word wrap switches to a variable-height list with per-row metadata. Auto-width samples at most 256 current result rows. Releasing old results and disconnecting the Engine run in the background so bulk buffer deallocation does not block the main thread.

The toolbar’s **File** menu offers **Load Range…** and **Load All Records**. Load Range sets the offset and row count. When the total source row count is known, the dialog shows how many rows remain at the current offset. First and Last scroll within the current result.

## Read-only source guarantees

The application provides no in-place cell editing. Parquet sources open through read-only file interfaces. DataFusion applies sidebar-generated conditions and sorting to the imported Arrow base. There is no advanced SQL input or hidden legacy query compatibility API. Read-only source protection also uses independent output-path guards.

All application outputs use a session-wide source guard. It records the original absolute path, resolved path, and device/inode identity to block overwriting an opened source or its aliases through symbolic links, hard links, and directory symlinks. A source remains registered for the process session after its window or preview closes. Even deletion leaves its path reserved. Data export, media/cell Save As, reports, and preferences share this protection. A source named `preferences.json` cannot be overwritten by saving settings.

Outputs are written first to a temporary file in the destination directory. Successful, uncancelled writes recheck the target before committing with rename. `source_guard::save_copy` uses `create_new`, avoids following pre-existing temporary symlinks, and leaves unrelated temporary objects alone. Table exports share `export_snapshot`, which opens a same-directory temporary file with `create_new` and hands it to the relevant writer. Temporary names include a time-based nonce, process ID, and atomic sequence number. Source protection and cancellation are checked before writing and committing. Export uses the displayed snapshot with its fields and sorting, and a failed export does not truncate the destination to a partial file.

These guarantees apply to reads, queries, and output paths managed by the application. The app still writes requested derived copies, preferences, and temporary data. It does not change source file permissions or prevent other processes from modifying files. Path and inode checks provide application-level protection without offering OS-level isolation against malicious concurrent filesystem replacement. Regression tests cover ordinary aliases, closed sources, old previews, preference-path collisions, pre-existing temporary files/links, and cleanup after failed commits. See the tests in `source_guard.rs`.

## Checking large Parquet range reads

The generator refuses to overwrite an existing file. The source and logs live under `target/large-data`. The fixture physically writes every record without using a sparse file or padding the end. These programs mainly check range reads, nested values, derived outputs, and source protection. Full-load memory, progress, and GUI behavior require separate validation.

```sh
# Generate the source only if the approximately 5.52GB fixture is absent.
cargo run --locked --release --example make_large_fixture
cargo run --locked --release --example check_large_fixture
```
