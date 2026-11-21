# Application bundle size (2026-11-01)

## Baseline and composition

The baseline application UUID was `C89318B0-6CAD-3FD2-8EA6-2A819461A042`. The executable in `dist/Parquet Viewer.app` was **121,589,696 bytes**. Regular files in the bundle totaled **121,737,856 logical bytes**, about 116.1 MiB. Resources occupied about 144 KiB. Statically linked Rust, GPUI, Arrow, and DataFusion machine code accounted for most of the bundle.

The Mach-O `__TEXT` segment occupied 115,998,720 bytes on disk, including 91,518,800 bytes of `__text`, unwind information, and read-only data. `__PAGEZERO` reserves virtual address space and adds no 4 GB file to the bundle. The build retains unwind information because the untrusted Arrow IPC metadata parser uses `catch_unwind`. Baseline hashes, segments, and files are recorded in `target/aggregation-package-before.json`. The original executable and source are in `target/aggregation-baseline-20261101`.

## Implementation

- The app continued to read and write Parquet through the `parquet` crate, retaining its codecs and canonical extension handling. The unused DataFusion `parquet` datasource feature was removed. The lockfile dropped the optional datasource and its unused dependency edges. Package versions stayed the same.
- `src/datafusion_session.rs` built the shared context from an empty `SessionStateBuilder`. It registered the scalar functions used by the filter protocol, the nested and core expression planners, and the Count, Sum, Mean, Min, and Max aggregates. NUnique used exact distinct counting. The optimizer and parallel physical plans remained available without registering every file format, UDF, or window UDF.
- Release builds kept O3, ThinLTO, stripping, and unwind support. They used no UPX runtime wrapper and retained existing filters, precision, and Parquet codecs.
- The new `--archive` option used macOS `ditto` to preserve the bundle structure and resource attributes, then checked ZIP CRCs before publishing the archive. Download size and unpacked size were measured separately.

Several internal DataFusion 54.1.0 dependencies enable their own defaults. Disabling math or string features in the top-level manifest alone may have little effect. Limiting actual registrations allowed the linker to discard unused implementations. The results below are file measurements.

## Reproduction and measurement

```sh
# Build the app and ZIP.
scripts/bundle-macos.sh --archive

# Reuse a verified release executable built from the same source and feature graph.
scripts/bundle-macos.sh --skip-build --archive

python3 scripts/inspect-package-size.py "dist/Parquet Viewer.app" \
  --archive "dist/Parquet Viewer-macOS-arm64.zip" \
  --baseline-binary target/aggregation-baseline-20261101/parquet-viewer-before \
  --output target/package-size-final.json
```

The inspection script reports executable size, total logical bytes in regular files, allocation from `st_blocks`, and the ZIP size, expanded entry bytes, and CRC results. APFS compression, clones, and shared extents can affect physical usage. Regular-file lengths alone do not account for directory metadata, resource forks, or shared storage. The `--baseline-binary` comparison covers only the two executables.

SQL tests for the reduced context exercised existing function aliases, Unicode text, Struct/List, exact Decimal and Date casts, grouped Count Distinct, and NULL handling. The suite recorded **212 passed, 0 failed, 1 ignored** (audio output hardware). Strict Clippy passed. Eight fixtures covered 14,703 rows, 156,344 cells, and 716 sort checks. Ten typed and eleven nested filter checks passed, and the source files were unchanged. Logs are in `target/aggregation-tests-final.log`, `target/aggregation-fixtures-validation.json`, `target/aggregation-typed-validation.json`, and `target/aggregation-nested-validation.json`.

## Results

The final Apple Silicon app UUID was **`CF10F958-9537-3072-B3F0-470D6A58761D`**. Both executable measurements used signed binaries inside the bundles so signature padding was included consistently. One MiB is 1,048,576 bytes.

| Measurement | Baseline | Final | Change |
|---|---:|---:|---|
| Regular files in the app, logical bytes | 121,737,856 bytes (116.10 MiB) | 101,896,496 bytes (97.18 MiB) | Down 19,841,360 bytes (18.92 MiB), **16.30%** |
| Signed executable | 121,589,696 bytes (115.96 MiB) | 101,748,336 bytes (97.03 MiB) | Down 19,841,360 bytes (18.92 MiB), **16.32%** |
| Distribution ZIP | Unmeasured | **34,557,643 bytes (32.96 MiB)** | No baseline ZIP measurement |

The ZIP at `dist/Parquet Viewer-macOS-arm64.zip` had SHA-256 `5e5dc493e0dd04c2814f47b893400d5e9b50a45d1b7157971566401cded7e8d3`. All entry CRCs passed. The archived executable and Info.plist matched the final bundle. Expanded entry sizes also include archived resource attributes, so the direct measurement of regular files provides the unpacked application size.

**The download was 32.96 MiB and the unpacked app was 97.18 MiB.** The comparable bundle measurement fell 16.30%. Runtime memory and speed require their own measurements on equivalent workloads.

The exact sizes, hashes, Mach-O UUIDs, segments, and file lists come from `target/aggregation-package-before.json` and `target/aggregation-package-final.json`. The baseline report measured the old bundle directly. The final report's baseline comparison covers the executable only. The bundle row above uses the two independent bundle measurements.
