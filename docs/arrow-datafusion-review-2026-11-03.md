# Arrow and DataFusion review and optimizations (2026-11-03)

This release removed a full-table identity index from the filter provider and improved array reuse during export and explicit materialization. Contiguous rows share Arrow slices, and eligible flat sparse ranges from a single source batch use Arrow take. Reading, filtering, sorting and aggregation continue to use Arrow and DataFusion. The measurements below report the before/after impact.

## Toolbar captions

All sixteen toolbar commands retain their icon and text. GPUI text measurement determines whether the toolbar fits in a single 36 px row. Narrow windows use a 36 px title/file row and a 30 px data/navigation row, totaling 66 px. The filename occupies the flexible drag area. Menus, modal and loading overlays, and axis context menus position themselves using the actual toolbar height.

In the two-row layout, Settings, file actions, the filename, Metadata, Export, Help and window controls occupy the first row. Fields, Filters, Clear Filters, Summary, Range, All, First and Last occupy the second. Tooltips retain the full descriptions and shortcuts. The final GUI section identifies which interactions were exercised.

## References and pinned APIs

| Reference | Use in this change |
|---|---|
| [DataFusion custom table providers](https://datafusion.apache.org/library-user-guide/custom-table-providers.html) | Produce batches from partition streams and describe projection, partitioning and order to the optimizer. |
| [DataFusion 54.1.0 ExecutionPlan](https://docs.rs/datafusion/54.1.0/datafusion/physical_plan/trait.ExecutionPlan.html) | Reuse existing operators and return a cancellable RecordBatch stream. |
| [DataFusion configuration](https://datafusion.apache.org/user-guide/configs.html) | Verify existing batch, partition, parallel sort and aggregation settings before attributing gains to changes. |
| [Arrow RecordBatch](https://arrow.apache.org/rust/arrow_array/struct.RecordBatch.html) and [take_record_batch](https://arrow.apache.org/rust/arrow_select/take/fn.take_record_batch.html) | Share arrays for contiguous rows and use typed kernels for sparse rows. Take allocates result arrays. |
| [Parquet ArrowReaderOptions](https://arrow.apache.org/rust/parquet/arrow/arrow_reader/struct.ArrowReaderOptions.html) | Check how OffsetIndex can skip pages for partial row ranges. |

Apache's online documentation tracks multiple releases. The code kept **Arrow/Parquet 58.4.1 and DataFusion 54.1.0**. The APIs were cross-checked against local sources, including both DataFusion streaming modules (catalog and physical plan), Arrow take, RecordBatch slicing and Parquet reader options. The review did not upgrade dependencies or reintroduce DuckDB.

## Implementation

### Generate identity values on demand

Previously, the provider allocated one UInt64 identity value for every loaded row before the first filter or sort. The cached MemTable retained that column alongside the query's filtered or sorted result index. The new provider uses `StreamingTable`, `PartitionStream` and `RecordBatchStreamAdapter`. It retains shared source RecordBatches and each batch's starting offset. DataFusion partitions generate identities and compatible sort keys as each batch executes. The main query emits base row indices. DataFusion evaluates predicates and provides stable sorting and ordered merging.

Removing the full-length identity saves about **8 bytes per loaded row**, plus object overhead, in logical allocations. Process RSS also reflects retained result indices, operators' temporary inputs, larger allocations shared by slices, and the allocator's retained pages. Refiltering regenerates identities at a CPU cost. Because this work moved from serial preparation into parallel execution, timing comparisons must include the complete `Engine::query` call.

Original row numbers, nonzero offsets, NULL and stable ordering across equal keys retain their semantics. Previous snapshots survive errors and cancellation. Aggregation still replays the pure filter with MemTable projection and requires no identity column. The Tokio runtime and filter SessionContext were already reused.

### Contiguous exports and same-batch gather

Exports already projected columns, iterated bounded batches and sliced a single contiguous range. The revised path also splits contiguous output at index-chunk and source-batch boundaries and returns `RecordBatch::slice` directly. Crossing a boundary no longer requires copying contiguous values just to fill an export batch.

Flat sparse, reordered or repeated indices in a single source batch can use Arrow `concat` (only for multiple bounded index slices), `numeric::sub` and `take_record_batch`. This reduces Rust HashMap lookups and per-row `(batch, row)` address pairs. Eligible types include primitives, Boolean, Null, String/Binary and relevant view or fixed-size Binary forms. Struct, List, Map, Union, Dictionary, RunEndEncoded and other nested/encoded types retain Arrow interleave. General cross-batch ranges also use interleave. Partial ranges within sparse chunks retain their existing fallback where necessary for range semantics and contiguous slicing. Contiguous slices remain available for every type.

A rejected prototype used take for nested sparse rows. It was faster but raised requested Rust allocation traffic from **35.07 MB to 88.61 MB** (decimal, median of the three per-process warm medians). In `arrow-select 58.4.1/src/take.rs`, `take_list` estimates child capacity from retained `child_data.len()` and the parent slice length. Small parent slices can still share a large child, inflating the estimate. The final implementation routes nested sparse ranges to interleave before allocating concat/sub indices. A 65,536-row Struct(List) regression selected seven rows and confirmed that the output child buffer was bounded by the fourteen selected values. Prototype data in `target/arrow-df-review-prototype-20261103/` is separate from the release measurements.

Index validation's existing Arrow maximum is reused. `OnceLock` computes the minimum and strict contiguity on demand for export, avoiding an additional scan for normal filtering or visible-cell access. Each index chunk carries a small amount of shape metadata, included in ArrowTable memory accounting. First export pays for its contiguity check. Pointer tests establish buffer sharing on contiguous paths only.

## Resource policy and alternatives

Loaded values remain in RAM as Arrow arrays. The UI renders the visible region. Each window may use all available CPUs. GPUI uses the GPU for rendering; query execution remains CPU-based. Shared Arrow buffers during filtering/sorting, chunked result indices, projected field caches, streaming export and parallel aggregation all predate this release.

Threads, batch sizes and workspaces were not increased. Caching arbitrary physical plans would require correct invalidation across constants, fields and sort shapes and could retain Arrow references longer. These alternatives would need profiling evidence.

**OffsetIndex was a candidate at the date of this review.** Parquet 58.4.1 offers optional index reads and page locations for skipping partial ranges. This primarily affects file I/O, with no direct benefit to filtering resident rows or to the memory needed for a full import. Enabling it indiscriminately adds metadata reads and RAM. Adoption would require demand-based caching, compatibility with missing or damaged indexes, and dedicated I/O comparisons.

## Regression and comparison results

The final suite reported **218 passed, 0 failed, 1 ignored** (audio hardware), recorded in `target/arrow-df-review-final-tests.log`. The 15 focused engine tests and 20 corrected columnar tests are in `target/lazy-index-engine-tests.log` and `target/arrow-gather-corrected-tests.log`. Coverage included identity lifetime, cross-batch and nonzero offsets, sparse indices, NULLs, stable ordering, cancellation rollback, shared contiguous buffers, nested NULLs, duplicate indices and bounded output from slices sharing a large child. The earlier 19-test record predates the correction.

### Method and scope

The [comparison JSON](../target/arrow-df-review-comparison-20261103.json) and [runner record](../target/arrow-df-review-runner.json) identify binaries, source and frozen workloads. Tests ran on arm64 macOS 27.2 with eight logical CPUs and Rust 1.88.0. Query and gather each ran three independent processes per version in the order before-1, after-1, after-2, before-2, before-3, after-3. Each version ran the same workload body. No coordinating builds or GUI actions ran during paired measurements. External desktop activity and OS/CPU cache state were neither monitored nor controlled. Three samples cannot establish statistical significance or a general speed guarantee.

The query source was a **10,000,000-row, single-column UInt64 Parquet file** of 20,011,858 bytes. `Engine::query` timing excluded publishing and dropping prior results, formatting values for verification and export. All runs matched expected row counts, cross-batch anchors and source row numbers. Outside timing, a 32,345-row descending Parquet export was compared value by value. Source size and mtime were unchanged in all six runs. The index accounted for a relatively large share of this narrow workload.

### Query time and resident memory

Raw timings are in trial order. Each row has its own median.

| Operation | Before (ms) | After (ms) | Median, before → after |
|---|---|---|---|
| First filter | 123.996 / 113.773 / 112.743 | 117.497 / 106.769 / 108.790 | 113.773 → 108.790 (−4.38%) |
| Repeat same filter | 105.825 / 107.113 / 108.975 | 105.144 / 107.108 / 107.551 | 107.113 → 107.108 (similar) |
| Descending sort after filter | 286.728 / 268.951 / 242.217 | 240.825 / 240.742 / 243.688 | 268.951 → 240.825 (−10.46%) |

Median first provider setup went from **5.846500 to 0.553125 ms**, while the index-query phase went from **105.445166 to 105.704000 ms**. Setup alone excludes the identity generation now performed during execution. The repeated filter changed little and the old sort timings varied. Loading code was unchanged, so 16.254 → 15.138 ms serves only as an environmental comparison. Sub-millisecond changes in clearing are also excluded from the main gains.

Removing the 10-million-row provider identity eliminated roughly **80 MB** of full-length logical allocation. The OS measurements below use decimal MB (1 MB = 1,000,000 bytes).

| Metric | Before median | After median | Reduction |
|---|---:|---:|---:|
| Physical footprint after publishing first filter | 264.242 MB | 185.042 MB | 29.97% |
| Physical footprint after publishing repeated filter | 344.982 MB | 265.553 MB | 23.02% |
| Observed five-stage physical-footprint peak | 736.380 MB | 659.031 MB | 10.50% |
| Observed five-stage RSS peak | 760.660 MB | 682.902 MB | 10.22% |

Footprint after first filter publication fell by about **79.20 MB**. Loaded values and result indices were retained. Later phases need not return the same amount of memory immediately. Individual five-stage footprint peaks were **733.759 / 736.380 / 737.953 MB** before and **659.031 / 656.852 / 660.456 MB** after. Peak here is the maximum of the before-stage, after-publication and periodic observations. A nominal 5 ms sample can miss short peaks. RSS, footprint, Arrow logical allocation and allocator retention measure distinct quantities.

Across all six processes, sampled peak simultaneous private query spill occupancy was **0** in both logical and allocated bytes. Cumulative disk writes were not measured.

### Arrow gather and export iteration

The workload held 262,144 rows, with 8,192-row source batches and 32,768-row index chunks. Three cases had four columns including Struct(List); the flat sparse case selected three columns. Source and index setup and exhaustive value/NULL/nesting checks were outside timing. Each process ran one cold metadata iteration followed by five warm iterations. Warm results use the median of five within each process, followed by the median across three processes.

| Case | Cold ms, before → after | Warm ms, before → after | Warm requested allocation traffic | Extra live peak, before → after |
|---|---:|---:|---:|---:|
| Contiguous iteration | 8.104 → 0.155 | 7.877 → 0.010 | 32.759 → 0.027 MB | 1,052,188 → 688 bytes |
| Same-batch nested sparse | 9.221 → 9.323 | 8.670 → 8.817 | 35.070 → 35.070 MB | 1,053,620 → 1,053,620 bytes |
| Same-batch flat sparse | 6.438 → 2.794 | 6.289 → 2.112 | 24.003 → 22.557 MB | 706,376 → 706,772 bytes |
| General cross-batch fallback | 13.563 → 13.586 | 12.043 → 12.110 | 33.135 → 33.135 MB | 1,031,640 → 1,031,640 bytes |

The contiguous path avoided copying values. Flat sparse warm median fell by 66.43% and requested allocation traffic by 6.03%, while extra simultaneous live allocation rose by **396 bytes**. Nested sparse allocation traffic and live peak were unchanged, with a roughly 1.69% higher warm time. General fallback warm time rose about 0.55%. Across the three process medians, flat sparse ranged from 6.091–6.312 ms before and 2.046–2.143 ms after. Fallback ranged from 11.999–12.129 ms before and 12.003–12.894 ms after. The comparison JSON retains all five individual iterations.

These timings cover the **in-memory gather/export iterator**. File serialization, compression and disk writes are outside their scope. Cold refers to building export metadata for the view for the first time; OS cache temperature was uncontrolled. Requested allocation traffic and additional live allocations also differ from process RSS, footprint and disk space.

### Build and delivery checks

| Check | Recorded outcome |
|---|---|
| Full tests, formatting, strict Clippy | 218 / 0 / 1; `target/arrow-df-review-final-{tests,fmt,clippy}.log` |
| Old/new query and four gather comparisons | Three independent processes per version. Corrected raw data and comparison JSON checked. Rejected prototype kept separately. |
| Final release | Built, matching frozen source, UUID `398FAD39-C852-3FB9-983B-0B1FC85D863C` |
| Package | Regular files in .app totaled 101,864,224 logical bytes. ZIP totaled 34,533,292 bytes. See `target/arrow-df-review-package-final.json`. |
| Installation, signing, ZIP round trip | Installed `/Applications/Parquet Viewer.app` and ZIP contents matched dist. Deep/strict signature check passed. Records: `target/installed-arrow-df-review-20261103.json` and `target/arrow-df-review-final-checks.json`. |
| Final GUI | Loaded 200,000 rows, filtered to 60,000 with source row numbers, ran Count rows, navigated to both ends and exported Parquet. Reopening from Recent and loading all rows after relaunch also passed. |

### Installed GUI verification

The installed build `398FAD39-C852-3FB9-983B-0B1FC85D863C` opened `target/demo/aggregation.parquet` with the native macOS file picker, selected all nine fields and loaded 200,000 rows using **All**. Filtering category to East China returned 60,000 rows. The first source positions were 1, 4, 7 and 13, with ids 0, 3, 6 and 12. **Last** reached source row 199,999 / id 199,998. **First** returned to source row 1. Count rows produced 60,000 and the detail table remained visible. GUI timing labels were excluded from the before/after measurements.

Export saved the current results to `target/demo/arrow-df-review-gui-398F.parquet`. An independent Parquet reader and Arrow equality/filter kernels derived expected results from the source and verified every typed value, nested array and NULL for all 60,000 rows and nine columns. See `target/arrow-df-review-gui-export-validation.json`. This verified on-disk output, without timing an end-to-end before/after export.

The first open from Recent stalled during schema discovery. A thread sample and diagnostic copy located the wait in the system `File::open` call, before Arrow decoding. Selecting the same file through the picker loaded it successfully. After quitting the final build, verifying process exit and relaunching, Recent immediately opened field selection and Load All succeeded. No system-level cause was established and no changes were made to reading algorithms, system permissions or security settings to account for the stall. The final screenshot is `target/arrow-df-review-final-toolbar.png`. Diagnostic and QA copies exited. One installed window remained open with 200,000 rows. No caffeinate process was used for this check, and language/theme/date were restored to Simplified Chinese / System / Local.

Earlier measurements belong to their respective builds. Wide Struct/Binary inputs have different costs from this narrow index benchmark. Result value buffers, exact distinct counts and high-cardinality groups still consume RAM.
