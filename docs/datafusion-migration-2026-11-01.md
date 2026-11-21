# Arrow and DataFusion migration (2026-11-01)

This report covers the `BDF77E89` build at the end of the migration. The
[subsequent optimization report](projection-optimization-2026-11-01.md) identifies the later application build and describes its field cache and export projection changes. Tests and timings below apply to the migration build.

This change removed DuckDB from the runtime, Cargo dependencies, Parquet reader, filtering, sorting and export. The build pinned Arrow/Parquet 58.4.1, DataFusion 54.1.0 and GPUI 0.2.2, and retained Rust 1.88. The old source and binaries were archived in `target/migration-baseline-20261101` for local comparisons and excluded from the app bundle. The comparison build, `967F582A-969E-3E1D-8700-4C90FB612730`, already included the previous direct Arrow streaming optimization. The earlier 9.870 s CTAS path is outside this comparison.

## Data path at the time of migration

```text
Parquet footer → column projection / source row range → parallel row-group decoding
                                                             ↓
                                              unfiltered Arrow RecordBatches in RAM
                                                             ↓
                                              DataFusion MemTable sharing ArrayRefs
                                                             ↓
                                      filtering / stable sorting → UInt64 row-index view
                                                             ↓
                                         viewport rendering / Preview / batched export
```

An ordinary load started neither DataFusion nor Tokio and created no temporary query directory. The first filter or sort created a MemTable over shared buffers. Queries returned only row indices, avoiding copies of nested values or large payloads during sorting. With the same fields, offset and row count, subsequent filters, sorts and resets reused the unfiltered base without rereading Parquet. Base indices and the import offset identified each original row. Across partitions, cumulative positions followed the sorted list of discovered files. Helper columns stayed out of clipboard data, preview fields and exports. Unsorted records retained source order. Sorting was stable for equal keys, placed NULLs last and preserved case-insensitive Text ordering.

All loaded rows and fields remained in system memory. Keeping the unfiltered base after a selective filter allowed users to change or clear it. Viewport size therefore did not bound result memory. The first DataFusion context allocated an internal index of about 8 bytes/row. The result index could add up to roughly 8 bytes/row, plus sort and query workspace. Map sort-key containers shared their payload. Legacy INTERVAL values needed normalized keys only when selected for sorting. Changing or clearing that sort released the old keys.

Each window independently used all available CPUs. Rayon handled row groups and Tokio/DataFusion ran queries, without a shared cross-window resource budget. The DataFusion workspace used 256 MiB per worker, bounded to 2–4 GiB; operators supporting spill could write to a private temporary directory. Loaded Arrow RAM and total process RSS were outside this limit. Queries used the CPU and GPUI rendered with the GPU.

Load progress compared decoded rows with the window's total from the footer. Filtering and sorting reported their own stages and actual output counts, omitting percentages when the total could not be estimated. Cancellation, operation failure or a type-check failure on reload preserved the prior view and snapshot. Reload recompiled typed conditions against the new schema. The UI provided no SQL input.

## Compatibility and export

All four export formats read an immutable Arrow snapshot and materialized values in index order in batches of up to 8192 rows. Parquet used ArrowWriter. JSON streamed decimals and large integers without conversion to Float. XLSX used the writer's constant-memory mode and temporary storage. XLS retained its BIFF8/SST/CFB support and limits, including memory costs for shared strings and the workbook structure. Memory behavior therefore varied by format. Every format wrote a new staging file, rechecked cancellation and source protection, then committed atomically.

Partitions used lossless unification of the actual Arrow types, recursively merging nullability of Struct/List/Map children. Time could be promoted only to higher precision, with overflow checks. Binary conversion rejected length mismatches that a safe cast might silently convert to NULL. Incompatible types, Decimal scales and extension semantics produced explicit errors. File-level metadata could differ.

Canonical Parquet extensions supported JSON and UUID, including nested Fields and sources without `ARROW:schema`. JSON preserved the tree and exact integers. UUID retained original bytes, canonical text and the Parquet annotation. Legacy Parquet INTERVAL used a lossless 12-byte bridge, preserving months, days and milliseconds, original NULLs and logical annotations. Display, JSON String output and comparisons using month=30 days were checked against actual output from the old build. Ordinary data bypassed this adapter. Map kept its Parquet representation. JSON used an array of typed key/value entries for duplicate Map keys to avoid overwriting an object member. The JSON preview serves as a display representation and does not round-trip every physical type.

The migration removed the text-import and advanced SQL backends after their controls had been removed from the UI. At that point the release offered neither CSV import nor a hidden DuckDB fallback. It preserved technical labels, themes, fixed-height virtualization, freezing and marking, the compact toolbar, and the Filters and Preview sidebars.

## Measurement method

Tests ran on an Apple M3 with 8 logical CPUs, 16 GiB RAM and macOS 27.2, using a release/ThinLTO build. The old and new executables ran three alternating, sequential trials with identical sources and operations. Reports retain all observations and give medians. The OS cache was not cleared. Machine exclusivity, cold launches and cold disk caches were not part of the procedure.

`check_migration` used a 200,000-row id/details/note fixture: load all rows, filter numerically, change the numeric condition, filter a Struct child, filter Text, sort all rows descending, and clear. Each operation retained the previous displayed snapshot until replacement succeeded, then checked the count, endpoints and original row numbers. On a 5,523,882,977-byte, 2,600,000-row source, `check_load_all` verified a complete import, actual progress, cancellation after work began, export from the previous snapshot and the lifetime of a single-cell slice after dropping the Engine. These checks did not write to the source.

Operation timings cover queries alone. CPU, RSS, physical footprint and spill observations cover the complete launched CLI workload, including initialization, checks and cleanup. Sampling at 20 ms can miss short peaks. RSS, physical footprint and logical Arrow buffer bytes are separate measurements. Spill reports the sampled peak of simultaneously occupied space. A zero spill peak does not cover preferences, exports or unrelated temporary writes.

To repeat the comparison, keep the previous binaries and use [profile-load.py](../scripts/profile-load.py) to produce separate fresh output prefixes. [compare-migration.py](../scripts/compare-migration.py) checks source identity, row counts and anchors and records trial values, ranges, binary SHA-256 digests and Mach-O UUIDs. Build and profile the new executable with:

```sh
cargo build --locked --release --bin parquet-viewer --example check_migration --example check_load_all
python3 scripts/profile-load.py target/release/examples/check_migration \
  target/demo/load-all-progress.parquet target/new-profile
```

This architecture did not push value predicates into Parquet. A first filter over a large window with very few matches could spend more time loading the entire window than the previous pushdown scan. These results do not establish that all queries are faster. This release reused the existing UI and table virtualization. This report provides no FPS, p95 or GPU query-acceleration measurements.

## Final build and validation

The release, dist and independent QA apps all had Mach-O UUID **`BDF77E89-A5EE-3ED5-B603-2B74B2DBE52B`**. The final CLI UUIDs were `94DF34A3-E490-300B-AC30-6A31976C63BB` for interactive workloads and `F9DD02FD-7F99-3761-8ACE-FA713F2948CF` for load validation. The dist and QA copies passed strict deep signature checks. This build was not installed into `/Applications`, signed with Developer ID or notarized by Apple.

- Final source: **193 passed / 0 failed / 1 ignored** out of 194 (audio output hardware). Formatting and strict all-target Clippy checks passed.
- Ten typed filter cases and 11 real Struct/List queries passed. Additional coverage included Decimal, large integers, nanosecond timestamps, parent and element NULLs, cancellation recovery and independent Engines.
- Across the eight fixtures then in use, 14,703 rows and 156,344 cells matched exactly after Parquet export and reopen. All 716 ascending/descending order, stable-order and source-row checks passed. Source SHA-256, sizes and mtimes were unchanged.
- An independent import of the same eight fixtures with the archived Engine matched the new build for text, NULLs, bytes, JSON and source row numbers. One Struct label gained quotation marks around field names, the only schema display difference.
- Dedicated regressions covered JSON/UUID without `ARROW:schema`, the original 12B INTERVAL payload, nested nullability, lossless partition type promotion and overflow rejection. The eight original fixtures did not cover every special type.
- The final GUI loaded 2.6 million rows (1943 ms on the label) and showed `786432 / 2600000` at 30%. First/last row navigation, descending id sort, two range conditions, clearing those conditions, export and two independent windows passed.
- In the final GUI, List contains 28 returned id=27/original row 28. Other checks covered the List table above JSON `[27,null,28]`, exact Decimal JSON in a Struct, a 680-byte Photo SVG, Esc dismissal, dragging over multiple rows and columns, and freezing both axes together.
- The three GUI-exported rows read back with ids 2599999, 2599998 and 2599997, and just the original three fields. Parquet re-export/reopen and six sorting checks passed.

GUI timing came from the displayed label and is excluded from the controlled sequential CLI comparison below. Full audio playback, all theme combinations and FPS/p95 were not rerun for this package. The independent QA app exited and the temporary caffeinate process stopped.

## Final performance comparison

The [machine-readable report](datafusion-migration-validation.json) includes raw values, ranges, binary identities and the validation index. The comparison covers complete application paths, including parallel decode, cache reuse and index views. It does not rank the two query kernels in general.

| 200,000-row query | Old median | New median |
|---|---:|---:|
| Load all | 117.751 ms | 43.657 ms |
| Numeric range filter (first query in process) | 12.980 ms | 2.468 ms |
| Change numeric condition | 9.780 ms | 0.560 ms |
| Filter Struct child | 2.961 ms | 2.941 ms |
| Exact Text match | 3.516 ms | 0.535 ms |
| Sort all records descending | 136.232 ms | 3.701 ms |
| Clear filters and sort | 112.308 ms | 0.032 ms |

The dedicated 5.5 GB / 2.6-million-row load check went from a median **2822 → 1565 ms**, a **44.5%** reduction. The old trials took 3109, 2797 and 2822 ms; the new trials took 1637, 1565 and 1499 ms. The new 200,000-row numeric filter trials took 13.795, 2.468 and 2.347 ms, retaining initialization and scheduling variability. The Struct condition changed little (2.961 → 2.941 ms). Clearing all filters and sorting reused the base data directly. Its short query time does not measure total UI response.

| Resource medians for complete workloads | Old | New |
|---|---:|---:|
| 200,000-row interactive workload · kernel-recorded peak RSS | 1.798 GB | 0.910 GB |
| 200,000-row interactive workload · sampled peak physical footprint | 1.673 GB | 0.703 GB |
| 200,000-row interactive workload · CPU | 0.879 s | 0.255 s |
| 2.6-million-row load check · kernel-recorded peak RSS | 3.057 GB | 3.871 GB |
| 2.6-million-row load check · sampled peak physical footprint | 6.418 GB | 6.253 GB |
| 2.6-million-row load check · CPU | 2.842 s | 2.766 s |

Memory figures use decimal GB. Median peak RSS for 200,000 rows decreased by about 49.4%. For the large file, sampled peak physical footprint decreased only about 2.6%, while peak RSS increased. macOS compression, resident allocations and 20 ms sampling affect these metrics. The large file used 5,632,191,967 logical Arrow bytes on the old build and 5,665,271,867 on the new one. Batch boundaries and buffer layouts account for the different overhead. Both builds loaded the same rows and fields. Both successful paired workloads had a zero sampled peak for private spill space. These observations do not establish a reduction in the 2.54 GB of spill from the earlier CTAS path.

In an additional full 2.6-million-row interactive workflow, the old build exhausted its 2 GiB workspace at `sort_all_desc`, after completing load and filter. The new build completed all three runs with a median sort time of **67.690 ms**. It sorted keys and row indices without rearranging the large Struct payload row by row. The old workflow did not finish, so there is no end-to-end speedup figure.

Raw profiling prefixes were `target/migration-final-small-*`, `target/migration-final-load-large-*` and `target/migration-final-large-interactions-*`. Earlier migration trials remain under target and are excluded from the final statistics. The independent fixture comparison is at `target/fixture-baseline-comparison-20261101-cells/comparison-report.json`, and the GUI export readback at `target/fixture-regressions-76682-1793510186602653000/report.json`.

Executable size increased from 47,076,080 to 121,540,736 bytes with the linked DataFusion Rust code. Avoiding duplicate data storage at runtime did not shrink the application bundle.
