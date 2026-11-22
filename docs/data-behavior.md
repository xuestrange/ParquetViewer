# Data behavior

Parquet Viewer uses Arrow and DataFusion to process data. The [design document](nested-and-import.md) covers nested data, loading, columnar storage and source protection.

## Type-aware filtering

Field selection, shortcuts, operator labels and input validation use the same schema type classification. A text value such as `00123` retains its type. Numeric inputs preserve Decimal and large-integer precision. Date supports on/before/after dates and inclusive ranges. Timestamp and Time compare complete values. The Date/Timestamp calendar changes only the date, preserving any time, fractional seconds and explicit zone. An empty Timestamp starts at midnight, with `+00:00` for zoned types. Struct fields use the type at their schema path. Primitive Lists test typed membership against complete elements, with case-sensitive Text matching and separate checks for empty collections and NULL.

Controls read existing field metadata without scanning data.

## Struct, List and in-window preview

Struct targets are expanded from the existing schema and cached. A condition addresses a root field and separate path segments. Names containing dots, quotes or spaces retain their identity. Each leaf uses comparison and input controls for its own type, while the Struct itself supports NULL checks. Rendering and draft editing require neither recursive schema parsing nor a scan of column values.

Primitive Lists support contains/does not contain an entire element, empty/nonempty collections and NULL/non-NULL checks. Number, Boolean, Date and Timestamp use exact typed literals. Text elements use case-sensitive whole-value matching. Ordinary text fields retain their substring filtering rules. An empty List differs from a NULL List. Lists of Structs, Maps, Lists and other unsupported element types offer collection-emptiness and NULL checks. Map, Binary and unrecognized fields support NULL checks.

Quick Peek shares the main window with Filters. Switching to Quick Peek preserves the filter draft. It displays a nested value as a table and JSON, and supports navigating child values, copying a node or the complete cell, and saving a value.

## Filter and query scope

A filter can apply to the loaded range or the entire file. For the loaded range, the engine evaluates typed conditions against the Arrow batches already in memory. A whole-file filter scans the source and retains the matching rows with their original row numbers. The Parquet scan can skip row groups when their statistics rule out a match.

To filter text across the source, open **Filters** and choose **Entire file**. Click **Add condition**, select a text field, choose **Contains**, enter the text, and click **Apply**. For example, `paris` matches both `Paris` and `Paris, France`. Use **Equals** to match the complete text value.

The sidebar generates typed DataFusion expressions. Struct fields use distinct path segments, and Lists match complete elements in their original type. Decimal and large integers avoid intermediate Float conversion. Date, Time and Timestamp input is validated against the relevant Arrow type. Ordinary Text filters and sorting ignore case, while Text List membership is case-sensitive. Equal sort keys use base indices to maintain stable source order. Each sort rule can place NULLs first or last. Map and nested Map sort keys adapt only the container metadata over shared buffers to a list of key/value structs. Source data remains Map.

## Filtering large datasets

Source metadata locates the requested row range and skips disjoint partitions and row groups. The Parquet reader projects columns and selects rows. A per-window Rayon pool reads intersecting row groups in parallel, and the merge restores source order. Some encodings may still require decoding skipped values. Parallel groups do not guarantee full CPU utilization for every file.

For a given range, the engine reuses the loaded fields and reads any additional fields requested. A filtered or sorted view can reference base rows by index while sharing their values. An explicit reload reads the source again.

Row numbers identify source positions. Single files start at `1`. Partitions accumulate row counts in the sorted order of discovered files. Filtering retains gaps and sorting preserves each record's original number. Copy and export use the selected data fields and omit internal row identifiers.

## Grouped summaries

Open **Statistical Analysis → Aggregation**, choose the measures, and select **Run aggregation**. The summary includes every row in the current result, including rows outside the viewport. Leave the grouping fields empty for one overall summary, or choose one or more fields for a summary per distinct combination of keys.

**Count rows** includes every row. **Count** counts non-NULL field values, and **NUnique** counts distinct non-NULL values. An empty string counts as a value. A NULL grouping key forms its own group, separate from the text `"NULL"`. Sum and Mean ignore NULL values and return NULL when the group has no non-NULL values. For integer and Decimal inputs, the engine widens the accumulator and reports an error if the required precision exceeds Decimal256 capacity. See [aggregation.rs](../src/aggregation.rs).

## Partition schemas and logical types

Partition merging compares actual Arrow DataTypes, field names and ordering. It recursively merges nullability in Struct/List/Map and other containers. Accepted common representations are demonstrably lossless, such as Null with a concrete type, compatible Binary storage, increased Time precision and Float16→Float32. Arrow cast performs conversions. Overflow, incompatible types or incompatible extension metadata produce explicit errors instead of NULL values. Identical `BLOB`, `TIME` or `FLOAT` UI labels do not guarantee identical storage. Schema-level metadata comes from the first partition. The other partitions retain their own summaries. Arbitrary schema evolution and lossy coercion are outside this contract.

Canonical Parquet extensions identify JSON and UUID from logical annotations even without an embedded `ARROW:schema`. Field metadata is preserved recursively at the top level and in Struct, List and Map keys/values. JSON export preserves the tree and exact integers. UUID uses canonical text for display and JSON. Its original bytes are retained, and the Parquet writer preserves the logical annotation. Shared physical storage alone does not allow JSON/UUID partitions to merge with ordinary VARCHAR/BLOB.

Legacy Parquet INTERVAL keeps all 12 bytes as `FixedSizeBinary(12)` with logical metadata. Choosing a single Arrow Interval unit could discard other components. Display, preview and JSON/Excel preserve legacy canonical text such as `1 month`, `30 days` and `00:00:00.001`. JSON uses a String. Parquet export keeps the bytes and restores the INTERVAL annotation. Only a selected sort field containing legacy INTERVAL triggers lazy generation of a Decimal128 comparison key using month=30 days and day=24 hours. The original month/day/millisecond values remain intact. Ordinary filtering, sorting other types and unsorted paths do not scan INTERVAL columns or allocate these keys. JSON, UUID and INTERVAL retain NULL-only value filters.

## Large results and progress

Imported values reside in system memory as a typed Arrow base. Two-axis virtualization limits UI elements and GPU drawing while retaining the base arrays. Filter and sort views share values with the base. Row indices use 32 or 64 bits depending on the size of the base. Sort keys, query workspace, nested JSON, media and export writers may consume additional memory.

For a Parquet range load, progress reports decoded rows against the window total from metadata. During a whole-file Parquet filter, progress counts examined rows, including row groups skipped using statistics. Sorting reports emitted rows while the final count is unknown. If a query is canceled or fails, the previous displayed result stays available.

## Date and binary display

An unquoted, unescaped `%` in a custom format selects chrono/strftime, for example `%Y-%m-%d %H:%M:%S`. Other formats support common .NET patterns such as `yyyy-MM-ddTHH:mm:ss.FFFFFFF`.

The .NET subset supports `y` through `yyyyy`, `M` through `MMMM`, `d` through `dddd`, one or two `H/h/m/s` characters, one to seven `f/F` characters, `t/tt`, `z/zz/zzz` and `K`, with single- or double-quoted literals and backslash escapes. `F` drops trailing zeros and, if all are zero, the preceding dot. .NET patterns show up to seven fractional digits. ISO/chrono can preserve nanoseconds. Adjacent quoted literals concatenate per .NET rules. Use a backslash or the other quote delimiter for a literal apostrophe.

The supported calendar is Gregorian, years 1–9999, with fixed English month, weekday and AM/PM names. Separators do not depend on system culture. Eras, standard single-letter date formats and .NET's `%` single-specifier shorthand are unsupported. Without a source time zone, `K` is empty and `z` does not infer the machine offset. Invalid or unsupported patterns and missing date/time components report conversion failure without panicking on the render thread. [Microsoft's custom date/time format documentation](https://learn.microsoft.com/en-us/dotnet/standard/base-types/custom-date-and-time-format-strings)

Binary numbers use little-endian order. GUIDs use .NET mixed endianness and IP addresses use network byte order. Empty byte arrays and null differ. Display conversion leaves source files and underlying export values intact.

## Columnar reads, rendering and resources

The Parquet reader produces typed `RecordBatch` values directly. `ArrowTable` and its immutable snapshots share array buffers. The view reuses the base when no filter or sort is active. Indexed views look up values in the base on demand.

Loading more rows or fields increases resident memory. The viewer formats cells on demand. Ordinary display is limited to 2000 Unicode characters. Copy, save and export retain full values. The per-window `SharedString` cache has an 8 MiB budget while active and 1 MiB while inactive.

The table virtualizes both axes with a 120px horizontal buffer. Frozen panes determine their own visible ranges. Fixed-height rows require constant-size scroll metadata, create only visible elements, and avoid full-result layout rebuilds on resize or sidebar changes. Wrapped columns retain estimated heights and a dynamic-height cache with per-row metadata. Auto-width uses at most 256 stratified samples. NULL highlighting reads the Arrow bitmap without guessing from text or scanning the table. These choices reduce element creation and formatting without changing query range or exports.

Each window can use all available CPU workers for parallel Parquet reads and DataFusion queries. Each engine sets its DataFusion managed memory allowance to the worker count × 256 MiB, bounded to 2048–4096 MiB. Eligible operations may spill to temporary disk. Resident Arrow data, indices, caches, JSON, UI, media and writers also contribute to process memory. Multiple windows share the machine's CPU, RAM and disk.

## Export and source protection

Nested-table virtualization limits the number of visible elements constructed. Opening a structure materializes the cell's JSON and `NestedTable` in the background. Column discovery and copying the whole table may still traverse all children. The image-specific preview skips JSON. Explicit value copying borrows a BLOB's complete bytes from a shallow one-cell Arrow slice and uses the BinaryFormat captured when opening the preview. Photo STRUCT keeps complete JSON. A slice may retain buffers for its column in its source batch, but not other columns or batches. Pretty JSON below the table is generated and cached on open and navigation, without per-frame serialization. Its 256 KiB display budget includes the truncation notice. Truncated preview text need not be a complete valid JSON document. Copy Node and Save Value use complete content and may allocate more. See the [design document](nested-and-import.md) for detailed memory and preview boundaries.

CSV, JSON, XLS, XLSX and Parquet use the current immutable Arrow snapshot and the selected field order. Contiguous ranges share slices. Other ranges gather rows in batches of up to 8192. CSV uses UTF-8, empty fields for NULL and compact JSON for nested values. JSON writes one cell at a time. XLSX uses constant-memory worksheets and temporary storage. XLS retains SST/BIFF buffers. Freezing affects only the display. Exports follow query order. Every format creates a new temporary file with `create_new` in the destination directory. After success, cancellation and source protection are checked before rename. Compression or work inside the writer can defer cancellation to a batch boundary.

Maps with ordinary unique keys become objects in JSON/Excel. When formatted keys collide or repeat, an array of entries preserves their original types and values, e.g. `[{"key": original_typed_key, "value": value}, …]`. Nested Maps use the same rule. Copying preview JSON gives the display representation and can differ from exporting the raw snapshot. Parquet export preserves the original Map container and keys/values, excluding sort helpers.

Legacy `.xls` allows 256 columns, 65,535 data rows plus a header, and 32,767 UTF-16 units per text cell. The CFB v3 limit of about 2 GiB and available memory also constrain workbook size. Exceeding a size limit produces an explicit error. XLSX checks Excel's row and column limits and writes values beyond its reliable numeric precision as text. Use Parquet export to preserve Parquet logical annotations.

Parquet footers supply source row counts, schema, projections and pruning metadata. Sources remain read-only. Exports and preferences are written as separate derived copies and application files.
