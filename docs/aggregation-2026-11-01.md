# Grouped summaries (2026-11-01)

This release introduced DataFusion 54.1.0 aggregation in a **360-logical-pixel right sidebar**. It shares space and layout rules with filtering and previews. Running a summary preserves the detail table, its order and its original row numbers.

## Using aggregation

1. Choose fields and a loading range. Apply any detail filters, then open **Aggregation**.
2. Choose one or more grouping fields, then add measures and select a field and operation for each.
3. Click **Run**. Scroll the results horizontally or vertically to see additional columns or groups.

The input includes **every detail row in the current immutable snapshot** after applying the active filter to the loaded range. Scrolling the viewport has no effect on the input, and aggregation does not scan unimported rows. Leave the grouping fields empty for a single summary of the entire snapshot. Changing the detail table's sort order leaves the statistics unchanged. The schema determines the available operations. Numeric-looking text retains its text type. The interface has no SQL editor.

| Measure type | Available operations |
|---|---|
| Numeric, including Integer, Float and Decimal | Count, NUnique, Sum, Mean, Minimum, Maximum |
| Text, Boolean, Binary, UUID, JSON, NULL | Count, NUnique |
| Date, Time, Timestamp | Count, NUnique, Minimum, Maximum |
| Struct, List, Map, Union, INTERVAL or an unrecognized type | Unavailable for grouping and measures in this release |
| No field | Count rows |

`Count rows` includes rows with NULL values. `Count(field)` counts non-NULL values. `NUnique(field)` evaluates exact `COUNT(DISTINCT field)` and excludes NULL. Empty strings are values. NULL grouping keys form their own group, separate from the string `"NULL"`. Multiple keys group by the full tuple. Groups are sorted by their keys in ascending order, with NULL last.

Sum, Mean, Minimum and Maximum ignore NULL. If every value in a group is NULL, those four operations return NULL, while Count and NUnique return zero. Empty detail input yields one ungrouped summary with Count zero, or zero groups if grouping keys were selected. Field names containing dots, quotes, spaces or SQL keywords are treated as complete names. Repeated measures receive distinct output titles.

## Execution and precision

The snapshot retains the underlying Arrow arrays and the original query's validated, side-effect-free scalar filter. DataFusion replays that filter against the shared arrays, projects the relevant fields, aggregates in parallel and sorts the groups. This avoids gathering an entire filtered or sorted detail view into a second value table or converting every value to Cell or JSON. Projection pushdown excludes unused wide Struct/Binary fields from computation. Execution streams RecordBatch values and retains the resulting groups in Arrow. An internal Count rows measure verifies that the replay selected exactly the snapshot's row count. The result omits that verification field.

This design relies on immutable buffers and an allowlist of pure functions. See the [DataFusion 54.1.0 DataFrame API](https://docs.rs/datafusion/54.1.0/datafusion/dataframe/struct.DataFrame.html) for plans and record-batch streams, and [DataFusion aggregate functions](https://datafusion.apache.org/user-guide/sql/aggregate_functions.html) for general semantics. The table above and the pinned version's regression tests describe the combinations exposed by this application.

DataFusion 54.1 integer and Decimal SUM accumulators use wrapping addition. Before summing, the application widens integer and Decimal values losslessly to Decimal128 or Decimal256 using the source precision, scale and the worst-case number of accumulated digits for the snapshot's row count. Negative scales widen to scale 0 where representable. When the result cannot be proven safe within 76 digits, the operation reports an error. It does not scan individual values to guess their magnitudes.

Integer and Decimal Mean use the safely widened Decimal type. DataFusion's AVG rules typically add four places of scale up to the physical type's limit. This produces a finite-scale mean. Float uses DataFusion's Float64 aggregation with its NaN and Infinity semantics. Titles and type descriptions preserve large integers and Decimal values without conversion to f64.

Each Engine can use the available CPUs and its own greedy query workspace. Exact distinct counts require state proportional to cardinality, with additional state for each distinct measure. When groups approach detail-row cardinality, storing the result requires corresponding RAM. The sidebar formats and paints visible rows and columns, bounds long text and avoids building a full matrix of result strings.

## Lifecycle

Aggregation runs in the background and supports cancellation. Starting a run clears its predecessor. Failure or cancellation leaves the detail snapshot usable. Changing the loaded range, applied filter or fields, or reloading the source, clears any result from the previous snapshot and retains compatible draft settings. Closing the sidebar cancels work and drops result references, moving large result destruction off the UI thread. Imported arrays remain available if the source file is deleted, so an existing snapshot can still be aggregated.

## Validation recorded for this build

The source checks reported **212 passed, 0 failed and 1 ignored** (audio hardware). Formatting and strict Clippy for all targets passed. Parquet export and reopen preserved 14,703 rows and 156,344 cells across eight files. There were 716 successful sorting and source-row checks, ten typed-filter cases and eleven Struct/List filter cases. Aggregation regressions covered Int64/UInt64 extremes, exact Decimal(38,18) Sum/Mean, NULL versus literal NULL, multiple keys, duplicate measures, quoted field names, empty input, filtered ranges and deleted sources. Detail export remained available after errors and cancellation.

`target/aggregation-debug-validation.json` records debug CLI correctness. Its 200,000-row fixture contains category, id, amount, text, flag, day, score and Struct/List fields ineligible for aggregation. Four category groups were checked for Count rows, exact Sum(id), NUnique(id) and NUnique(text)=17. With offset 10, limit 100 and id ≥ 50, the expected result was 60 rows with Sum(id)=4770. Debug timings were excluded from release performance claims.

In the debug GUI build with UUID prefix **`78405F36`**, category + flag produced ten groups from 200,000 rows. Count rows, Sum(amount) and NUnique(text)=17 were checked. The Text measure menu offered Count and NUnique. Filtering category to East China left 60,000 detail rows with original positions including #1, #4 and #7. The summary consumed those 60,000 rows and produced three category + flag groups. The main table continued to show details.

## Release CLI measurements

The final application UUID was **`CF10F958-9537-3072-B3F0-470D6A58761D`**. Release examples built from the same source made three consecutive calls per source in one process on the same imported snapshot. The first included DataFusion runtime/context initialization. Subsequent calls reused the runtime but rebuilt and executed the plan. No summary results were cached.

| Loaded source and operation | First (ms) | Second (ms) | Third (ms) |
|---|---:|---:|---:|
| 200,000 rows, four category groups, ten measures | 25.064 | 6.815 | 6.657 |
| 200,000 rows, 200,000 id groups, four measures | 13.293 | 10.523 | 10.252 |
| 5.5 GB / 2,600,000 rows, no grouping, four measures | 324.587 | 49.740 | 40.691 |

The low-cardinality case measured Count rows, Sum/Mean/NUnique(id), Sum(amount), Count/NUnique(text), Minimum/Maximum(day) and NUnique(flag). The high-cardinality and large-source cases measured Count rows, Sum(id), Mean(id) and NUnique(id). The high-cardinality result retained all 200,000 groups, with **11,203,360 logical Arrow bytes**. The JSON records group count, data type and the first eight groups as anchors. Only those anchors were compared at the value level.

The 5.5 GB whole-table result was Count rows=2,600,000, Sum(id)=3,379,998,700,000, Mean(id)=1,299,999.5000 and NUnique(id)=2,600,000. The result occupied **432 logical Arrow bytes**. That figure excludes the Engine and other process memory. Source loading took **2271.279 ms**, recorded separately from aggregation.

Operation timings include the call and aggregation execution, and exclude source opening, loading and subsequent correctness checks. The OS page cache was not cleared. GUI frame rate and p95 latency were not measured. The previous build did not offer grouped summaries, so these runs establish no before/after speedup. The first call and later calls also have different scopes. Raw results are in `target/aggregation-release-validation.json`, `target/aggregation-high-cardinality-release.json` and `target/aggregation-large-release.json`. The 200,000-row case also verified the 60 filtered rows with Sum(id)=4770. All three sources retained their sizes and modification times.

The separate process sample in `target/aggregation-large-release-system.json` recorded **2.935 s** wall time, **3.859 s** CPU, **3.189 GB** kernel peak RSS, **3.135 GB** peak sampled RSS and **6.271 GB** peak sampled physical footprint. Sampling used 20 ms intervals and decimal GB. RSS, physical footprint and logical Arrow bytes are separate measures. The process included startup, source import, three aggregations, validation and teardown. These values cannot establish a single aggregation's memory peak, its incremental allocation or a reduction relative to an older version. Across 121 successful procinfo samples, peak observed private query spill occupancy was zero. Short peaks and other disk writes could fall outside the sampling scope.

## Final package and GUI checks

`target/aggregation-package-final.json` records the arm64 application and compressed distribution.

| Item | Measured logical file size |
|---|---:|
| Previous executable | 121,589,696 bytes (121.590 MB) |
| New executable | 101,748,336 bytes (101.748 MB) |
| Extracted new .app | 101,896,496 bytes (101.896 MB) |
| Distribution ZIP | 34,557,643 bytes (34.558 MB) |

The executable including aggregation was **16.3%** smaller than the `C89318B0` baseline executable. The previous application and ZIP sizes were not measured. The ZIP describes transfer size. Extraction requires approximately 101.896 MB of application file data, excluding directory and other filesystem metadata. APFS physical allocation can differ. ZIP CRC validation passed, and the executable and Info.plist matched the application bundle. Local ad hoc signing passed `codesign --verify --deep --strict`. The release had neither Developer ID signing nor Apple notarization.

| Final check | Recorded result |
|---|---|
| Release UUID, package and signature | `CF10F958-9537-3072-B3F0-470D6A58761D`; strict deep verification passed for the ad hoc signature |
| Release timings, resources and raw reports | Three calls for each of three sources, with separate first-call and subsequent scopes |
| GUI relaunch | Independent QA copy checked after relaunch, with the final release's `CF10F958` UUID prefix |

The relaunched QA copy opened all nine fields in the 200,000-row demo using Load All. Category + Count rows produced four groups. East China, North China, South China and NULL contained 60000, 60000, 60000 and 20000 rows respectively. The interface showed 19 ms. Decimal amount offered Count, NUnique, Sum, Mean, Minimum and Maximum. Sum returned **5454468190.25 / 5454468182.25 / 5454668167.50 / 1818222733.50** in the same order, with an 8 ms label.

Filtering category to East China left 60,000 details with source row numbers including #1, #4, #7 and #13. The previous summary was cleared and the category + Sum(amount) draft survived. Running again produced one group from 60,000 inputs with Sum(amount)=5454468190.25 and a 21 ms label. These values came from the interface. The CLI benchmark above measured a separate path; the GUI check did not measure end-to-end latency or frame rate.

The final GUI check confirmed that the main table still showed details, the independent QA copy exited (running state false) and the temporary caffeinate process stopped. Two grouping fields, the Text operation menu and horizontal scrolling were checked in the earlier debug run. The final GUI run did not repeat those checks or the full theme/media suite.

## Reproduction

```sh
cargo test --locked --offline --bin parquet-viewer
cargo fmt --all -- --check
cargo clippy --locked --offline --all-targets -- -D warnings

# Generate the dedicated GUI fixture if absent.
cargo run --locked --offline --example check_aggregation -- --make

# Run the correctness example; keep debug and release timings separate.
cargo run --locked --offline --example check_aggregation

# After a release build, check ungrouped statistics on an existing large source.
# Supply a field as the second argument to group by it. Cardinality affects work and result size.
target/release/examples/check_aggregation /absolute/path/to/source.parquet
```
