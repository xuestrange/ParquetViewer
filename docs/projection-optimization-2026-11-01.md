# Field caching and export projection optimization (2026-11-01)

This change reduced repeated work when switching fields and exporting a subset of columns on the Arrow + DataFusion architecture. The query engine stayed the same. The pre-change source, dependencies, and binaries were archived in `target/projection-baseline-20261101`. The baseline application UUID was `BDF77E89-A5EE-3ED5-B603-2B74B2DBE52B`. Performance was measured in an independent same-source comparison, separately from the preceding migration. The final application UUID was `C89318B0-6CAD-3FD2-8EA6-2A819461A042`. It completed build, signing, and independent GUI verification. The first intermediate package crashed when applying a field selection. The validation record below distinguishes that package from the subsequent fix.

## Two changes

**Cache the union of fields for the current range.** Each Engine retains one union of unfiltered Arrow fields for its current Offset/Rows range. Reducing or reordering fields already read uses an Arrow projection that shares the original arrays. When adding fields, the Parquet reader's ProjectionMask selects only columns not already decoded. If batch boundaries differ between reads, Arrow slices align the batches for merging without decoding or copying the whole wide table. Existing value buffers remain shared.

The displayed base and DataFusion MemTable contain only the fields in the current selection. Hidden cached columns cannot enter filters or exports. Changing fields rebuilds the provider. Subsequent filters and sorts with the same fields reuse the existing context and return only row indices. Source row numbers, pre-filter Offset/Rows semantics, and stable sorting are preserved.

**Project before gathering rows for export.** The writer first selects fields from the immutable snapshot. It then uses Arrow gather/interleave in batches according to result row indices. Unselected large Text, Binary, or nested fields never enter the gather operation or the writer. Batches contain at most 8192 rows. Contiguous unfiltered results can still share slices, without creating a full reordered result.

JSON, Parquet, XLSX, and XLS use the same projection entry point and export complete original values, including values clipped on screen. Regression checks for all four formats cover field order, repeated/reversed row indices, NULL, exact Decimal, nested values, and legacy INTERVAL. Parquet preserves the corresponding Schema/Field metadata. JSON and Excel keep their established type representations. For example, Excel stores exact Decimal and nested values as text, with no claim that they have Parquet's physical types. Existing costs include XLS row/column limits, XLSX writer temporary storage, and the XLS shared-string table.

## Memory and lifetime

Columns read and later hidden in the same range stay in RAM for reuse. The union can cover all fields in the source schema, but only one range is cached. Hiding columns does not release them immediately, and adding more fields can increase resident memory. A read of missing fields must retain the old display alongside the new columns and merge metadata. Shared Arrow buffers still have overhead and do not guarantee lower process RSS.

Changing Offset/Rows replaces the current range cache. Reload uses a new Engine to reread the current selection, allowing previously hidden fields to be reclaimed. Closing the Engine or dropping the last snapshot of that range releases cache references. Buffers still held by an old snapshot, preview, or export job remain alive as long as those owners need them. Hiding a field cannot guarantee an immediate return of memory to the OS. Each window can still use all available CPUs independently. This change introduced no cross-window budget or new display-memory limit.

A failed or cancelled operation restores the prior field union, current selection, and query context without publishing a partial snapshot. Older snapshots continue to support export. Already-read fields can still be projected and filtered after the source file is deleted. Adding an unread field requires a valid source. A missing source or changed identity/schema causes an error and retains the old result. Output is written to a staging file, then committed after success, cancellation checks, and source protection. Failure keeps an existing destination intact. The source remains read-only.

## UI scope and measurement boundaries

Applying a field selection in the UI exercises the field cache. The UI exports **all currently displayed fields**. No separate export field picker was added. The benchmark API can export only id or id/note from a snapshot that also contains a large nested column. That narrow selected-column case is measured separately from exporting all displayed fields in the UI. An all-field export has no unselected fields to skip, and the underlying writer costs still apply.

`check_projection_performance` ran the same steps against the same id/details/note source.

- `fields` read id/details initially, reduced the selection to id, reordered it, added note, hid and reselected fields, filtered and sorted, then exported an old snapshot.
- `export` loaded all three columns, filtered and sorted up to 100,000 rows in descending order, then exported and read back id and id/note in Parquet and JSON.
- `export-small` ran the same narrow exports for at most 5000 rows in JSON, Parquet, XLSX, and XLS.

Per-operation query/export timings exclude correctness verification. Full-process CPU, elapsed time, peak RSS, and physical footprint include startup, validation, snapshot retention, and release. The import stage may dominate the process peak, so that peak cannot isolate memory savings from export projection. Arrow logical bytes, RSS, and physical footprint are reported separately. Sampling every 20 ms can miss short peaks. Spill measurements refer to concurrent occupied space rather than cumulative writes. “Cold” in the example refers only to a fresh application cache. The operating-system page cache was not cleared.

## Same-source performance comparison

The local release build ran on macOS 27.2 / arm64 with 8 logical CPUs and Rust 1.88. Both versions used the same **200,000-row, 424,711,247-byte** Parquet source. Its SHA-256 was `9c58378ac1b3b054402d7945c265966a2bd92d154efd864634a6898d2617f33d`. Source device/inode, size, and mtime were unchanged before and after the workloads. The old and new example bodies had identical SHA-256 hashes. The CLI UUIDs were `FDEB42CD-495F-3D7C-AC41-831025B2E288` before and `35498066-63D2-3ADE-977D-6D1DE7680AE8` after.

Both `fields` and `export` ran three old/new pairs, 12 independent processes in total. Pairs ran serially in **BA, AB, BA** order, with B for the baseline and A for the new build. Builds finished and all Parquet Viewer GUI windows closed before sampling. OS caches were not cleared. The first run's initialization/scheduling variation was included. The [machine-readable comparison](projection-performance-2026-11-01.json) contains all three observations, min/max, and hashes of the binaries and raw reports. Execution order is in `target/projection-perf-20261101/{protocol,execution}.json`.

Timing tables use **milliseconds, median [min–max]**. They cover query/export calls and exclude value-by-value validation.

| Field-switching workflow, 200,000 rows | Before | After |
|---|---:|---:|
| Initial id/details read | 75.240 [57.524–155.451] | 55.723 [55.692–60.677] |
| Reduce to id | 0.755 [0.560–0.820] | 0.106 [0.102–0.228] |
| Reorder to details/id | 55.152 [52.677–69.950] | 0.093 [0.089–0.103] |
| Add note | 56.950 [54.652–100.011] | 1.091 [1.024–1.227] |
| Hide details/note | 0.668 [0.481–0.730] | 0.093 [0.087–0.155] |
| Reselect hidden fields | 56.413 [50.190–83.917] | 0.093 [0.082–0.200] |
| Filter and sort 1000 rows descending after field switches | 4.077 [3.971–121.420] | 4.075 [3.940–142.945] |

Avoiding repeated reads of the large details field accounted for the main improvement. Adding note retained the id/details buffers. The recorded evidence includes cache hits and overlapping addresses for data and offset buffers. Filter/sort medians were similar. Initial loading was outside the intended optimization, and its observed difference does not establish a consistent speedup. The submillisecond query measurements exclude the field panel, layout, painting, and user interaction, so they do not measure UI latency.

| Selected-column API export, 100,000 rows | Before | After |
|---|---:|---:|
| Parquet, id | 45.459 [32.503–70.396] | 13.279 [12.563–14.242] |
| Parquet, id/note | 37.309 [36.394–41.354] | 23.264 [23.128–23.478] |
| JSON, id | 47.325 [44.896–47.671] | 32.563 [30.454–42.509] |
| JSON, id/note | 69.203 [68.332–69.305] | 55.471 [53.749–60.522] |

Excluding the large details column from an indexed id/details/note snapshot reduced the four export medians by **70.8%, 37.6%, 31.2%, and 19.8%**, respectively. These are the selected-column API results. The experiment did not benchmark the UI's all-displayed-fields export. Exporting only id as Parquet from the original unfiltered snapshot changed little, from 18.691 to 18.313 ms. One old and one new `export-small` run also passed readback at 5000 rows for four formats × two field selections. Some JSON/Excel cases were slower in the new build. That single smoke test establishes correctness only and cannot estimate a stable XLS/XLSX speedup.

| Full CLI workload, median resource use | Fields before → after | Export before → after |
|---|---:|---:|
| Elapsed | 0.540→0.338 s | 0.526→0.426 s |
| CPU | 1.349→0.529 s | 0.683→0.620 s |
| Kernel peak RSS | 1.771→0.900 GB | 0.929→0.910 GB |
| Physical footprint peak sampled every 20 ms | 1.742→0.870 GB | 0.900→0.881 GB |
| Sampled peak simultaneous private query spill | 0→0 bytes | 0→0 bytes |

GB is decimal. The field workflow deliberately retained the first id/details snapshot to verify old-snapshot behavior and buffer sharing. It also retained the current display and, during replacement, the preceding display. The older build's repeated reads created more independent buffers. Median peak RSS fell by approximately 49.2% for this workflow. **That percentage does not establish the reduction in ordinary GUI resident memory.** Import may dominate full-process memory in the export workflow. The roughly 2.1% change in RSS/footprint cannot establish export-stage peak savings in isolation. Zero spill means the private query directory showed zero simultaneous allocated space at the 20 ms sampling points. This excludes export files and writer temporary storage and does not measure cumulative disk writes.

## Single correctness run on the 5.5 GB source

One separate new-build `fields` run used a real **5,523,882,977-byte / 2,600,000-row** source. Initial id/details loading took **3461.052 ms**, reduction to id **1.070 ms**, reordering **0.307 ms**, adding note **58.235 ms**, reselecting hidden fields **0.130 ms**, and the subsequent filter and descending sort of 1000 rows **68.744 ms**. There was no paired old-build run on this source. Comparing these times with the preceding project phase would not establish a speedup.

Each query checked every id and original row number and inspected Struct/List/NULL anchors across batch boundaries and at the beginning, middle, and end. All existing id/details data and offset buffers were shared. The first query's snapshot still exported an id-only Parquet with 2.6 million rows in the verified original order. Source device/inode, size, and mtime were unchanged. This run did not recompute a full SHA for that large source. The three fields accounted for **5,665,271,867** logical Arrow bytes. Hidden columns remained in the in-memory field union, so logical bytes for an id-only view do not represent the entire Engine's memory.

The complete process used **6.845 s** elapsed, **8.601 s** CPU, **3.026 GB** kernel peak RSS, and **6.163 GB** sampled peak physical footprint. There were 201 successful procinfo samples and zero sampled peak private spill. The CLI workload included validation and pinned snapshots. It measured neither GUI frame rate nor typical single-window resident memory. Raw results are `target/projection-large-fields-after.json` and its corresponding `-system.json`.

## Reproduction

The source validation and build commands were:

```sh
cargo test --locked --offline --bin parquet-viewer
cargo fmt --all -- --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline --release --bin parquet-viewer --example check_projection_performance

# Use the same sequential id/details/note fixture and a new output prefix for each run.
# This path reuses the existing 200,000-row fixture without rebuilding or modifying the source.
PV_PROJECTION_BENCH_MODE=fields python3 scripts/profile-load.py \
  target/release/examples/check_projection_performance \
  target/demo/load-all-progress.parquet target/projection-after-fields-1
PV_PROJECTION_BENCH_MODE=export python3 scripts/profile-load.py \
  target/release/examples/check_projection_performance \
  target/demo/load-all-progress.parquet target/projection-after-export-1
PV_PROJECTION_BENCH_MODE=export-small python3 scripts/profile-load.py \
  target/release/examples/check_projection_performance \
  target/demo/load-all-progress.parquet target/projection-after-export-small-1

# Separate 5.5 GB new-build correctness run, independent of the paired 200,000-row comparison.
PV_PROJECTION_BENCH_MODE=fields python3 scripts/profile-load.py \
  target/release/examples/check_projection_performance \
  target/large-data/struct-5gb.parquet target/projection-after-large-fields-1
```

The before binary was `target/projection-baseline-20261101/check-projection-before`. Substitute that path for the binary above and choose a fresh output prefix to repeat a baseline run. Both sides used **identical benchmark bodies**. The older `check_migration` and `check_load_all` programs in the baseline directory perform different operations. Both sides checked the same source identity, row count, every id and source row number, nested anchors, and export readback. The example itself does not recompute the source file's full SHA. Overlapping buffer address ranges establish sharing only for the reported Arrow data and offset buffers. They do not measure RSS, actual disk reads, or CPU saturation.

## Build and GUI validation

The full source checks reported **203 passed / 0 failed / 1 ignored** (audio output hardware). Formatting and strict all-target Clippy passed. Added regressions covered field projection, merging misaligned batches, buffer sharing, switching cached fields after source deletion, hidden-field isolation, rollback after changes or cancellation, old snapshot lifetimes, projection before gathering for the four formats, and target protection after failure.

The updated source was also exercised against the eight test files used at the time. Parquet export and reopen reproduced 14,703 rows and 156,344 cells exactly. All 716 checks for ascending/descending sorting, stable order, and original row numbers passed. Source SHA-256, size, and mtime stayed unchanged. Ten real typed filter cases and 11 Struct/List nested filter cases passed. Raw reports are `target/fixture-regressions-78845-1793514295107661000/report.json`, `target/projection-typed-filters.json`, and `target/projection-nested-filters.json`.

| Check | Result for this change |
|---|---|
| Strict all-target Clippy | Passed |
| Release build and signature | Passed, including strict deep ad hoc signature verification of dist and independent QA |
| Alternating same-source performance, CPU, RSS, physical footprint, spill metrics, and raw logs | The 12 comparisons and the single new-build large-file run passed. Full values are in the machine-readable comparison. |
| CLI UUIDs and binary identities | Baseline FDEB42CD, new 35498066. Full UUIDs and SHA-256 hashes are in the machine-readable comparison. |
| Final application UUID | `C89318B0-6CAD-3FD2-8EA6-2A819461A042`, identical in release, dist, and independent QA |
| Final package GUI field switching, filtering/sorting, hiding/reselecting, and export | Passed. The 200,000-row interaction and export readback are described below. |

The first intermediate package UUID was `63344150-1D79-357D-9579-E4FCE9478F48`. Both dist and the independent QA copy passed strict deep ad hoc signature checks. GUI testing found that Apply Fields cleared widths before the old table was replaced, causing an out-of-bounds paint. The BDF77 baseline also contained that code. The UI fix and final-package recheck are recorded below. The intermediate package did not pass GUI validation. The CLI did not exercise this UI path, so its measured backend was unaffected by the fix.

The fix updates column widths by field name when the new result commits. While waiting, after cancellation, and after failure, the old result keeps its old widths. Subsequent queries with the same fields retain their widths, and new fields use the existing defaults. Two new regression tests cover additions/removals, reordering, replacing fields without changing the column count, and invalid widths. The fixed debug test build UUID was `50AABCD5-577D-3AA0-8C79-164AFC8AF8E0`. A GUI run on 1000 rows checked hiding a Struct, adding note, reselecting the cached Struct, applying an unchanged selection, NULL, and table/JSON previews. Independent QA then restarted the final release for the following acceptance run.

Final independent QA on `C89318B0-6CAD-3FD2-8EA6-2A819461A042` loaded id/details for 200,000 rows, with a UI timing label of 147 ms. Reducing to id, adding note, reselecting cached details, and applying an unchanged selection all retained 200,000 rows without a crash. The reselect label was 0 ms, an integer-truncated backend timing that excludes full UI latency. After sorting id descending, filtering on `id >= 199997` returned 199999, 199998, and 199997 with original row numbers 200000, 199999, and 199998. The reselected Struct's same-window table and JSON previews worked. The first item had customer.id=99996 and city=`杭州` (Hangzhou).

The GUI exported all displayed fields for those three rows to `target/projection-final-ui/export.parquet`. Readback verified id, note, customer, two exact Decimal readings, the 2048-byte payload, and optional NULL. Parquet re-export and reopen reproduced nine cells exactly, and six sort checks passed. The raw reports are `target/projection-final-ui-validated.json` and `target/fixture-regressions-79914-1793516364551492000/report.json`. The final GUI pass covered field switching, a Number filter, sorting, Struct preview, and Parquet export. Earlier theme, freezing, multiwindow, and Photo checks retain their historical scope. Signature and SHA records are in `target/projection-final-fixed-bundles.json`. The final application used the tested data backend with the UI commit fix. Hashes of the four backend files remained unchanged and the CLI UUID stayed `35498066-63D2-3ADE-977D-6D1DE7680AE8`.

The preceding [Arrow + DataFusion migration report](datafusion-migration-2026-11-01.md) retains its original UUIDs, test counts, and performance measurements.

The [validation JSON](projection-validation-2026-11-01.json) records the complete delivery and acceptance checks. The QA window exited, temporary caffeinate stopped, and persistent power settings remained unchanged. This run updated the workspace's `dist` and left the existing `/Applications` copy in place. FPS and p95 were not measured. The observed speed and memory changes apply to the workloads described above.
