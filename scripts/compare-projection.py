#!/usr/bin/env python3
"""Compare a three-pair projection benchmark without conflating memory phases.

Usage: compare-projection.py RAW_DIR BEFORE_BINARY AFTER_BINARY NEW_OUTPUT_JSON
Requires fields/export-{before,after}-{1,2,3}.json and companion -system.json.
Only reads benchmark inputs/binaries; creates the output with exclusive mode.
"""
from __future__ import annotations

import hashlib
import json
import math
from pathlib import Path
import statistics
import struct
import sys
import uuid

MODES = ("fields", "export")
VERSIONS = ("before", "after")
RUNS = (1, 2, 3)
TIME_METRICS = ("wall_seconds", "child_cpu_seconds")
MEMORY_METRICS = (
    "child_peak_rss_bytes_macos",
    "max_sampled_rss_bytes",
    "max_sampled_physical_footprint_bytes",
)
SPILL_METRICS = (
    "max_sampled_spill_logical_bytes",
    "max_sampled_spill_allocated_bytes",
)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def read_json(path: Path) -> dict:
    value = json.loads(path.read_text())
    require(isinstance(value, dict), f"Expected JSON object: {path}")
    return value


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def macho_uuids(path: Path) -> dict:
    """Read LC_UUID without requiring Xcode/dwarfdump or executing the binary."""
    thin = {
        b"\xcf\xfa\xed\xfe": ("<", 32),
        b"\xfe\xed\xfa\xcf": (">", 32),
        b"\xce\xfa\xed\xfe": ("<", 28),
        b"\xfe\xed\xfa\xce": (">", 28),
    }
    fat = {
        b"\xca\xfe\xba\xbe": (">", 20),
        b"\xbe\xba\xfe\xca": ("<", 20),
        b"\xca\xfe\xba\xbf": (">", 32),
        b"\xbf\xba\xfe\xca": ("<", 32),
    }
    result = []
    with path.open("rb") as stream:
        size = path.stat().st_size

        def parse_slice(offset: int) -> None:
            stream.seek(offset)
            header = stream.read(32)
            require(len(header) >= 28 and header[:4] in thin,
                    f"Invalid Mach-O slice at offset {offset}: {path}")
            endian, header_size = thin[header[:4]]
            cpu_type = struct.unpack_from(endian + "I", header, 4)[0]
            commands, command_bytes = struct.unpack_from(endian + "II", header, 16)
            require(commands <= 65536 and command_bytes <= 16 * 1024 * 1024,
                    f"Unreasonable Mach-O load commands: {path}")
            require(offset + header_size + command_bytes <= size,
                    f"Truncated Mach-O load commands: {path}")
            stream.seek(offset + header_size)
            data = stream.read(command_bytes)
            position = 0
            found = False
            for _ in range(commands):
                require(position + 8 <= len(data), f"Truncated load command: {path}")
                command, length = struct.unpack_from(endian + "II", data, position)
                require(length >= 8 and position + length <= len(data),
                        f"Invalid load command length: {path}")
                if command == 0x1B:  # LC_UUID
                    require(length >= 24, f"Invalid LC_UUID: {path}")
                    arch = {7: "i386", 0x01000007: "x86_64", 12: "arm",
                            0x0100000C: "arm64"}.get(cpu_type, str(cpu_type))
                    result.append({"architecture": arch,
                                   "uuid": str(uuid.UUID(bytes=data[position + 8:position + 24])).upper()})
                    found = True
                position += length
            require(found, f"Mach-O has no LC_UUID: {path}")

        magic = stream.read(4)
        if magic in thin:
            parse_slice(0)
            return {"format": "Mach-O", "uuids": result}
        if magic in fat:
            endian, arch_size = fat[magic]
            count_data = stream.read(4)
            require(len(count_data) == 4, f"Truncated fat header: {path}")
            count = struct.unpack(endian + "I", count_data)[0]
            require(0 < count <= 64, f"Invalid fat architecture count: {path}")
            table = stream.read(count * arch_size)
            require(len(table) == count * arch_size, f"Truncated fat table: {path}")
            offsets = [struct.unpack_from(endian + ("Q" if arch_size == 32 else "I"),
                                          table, i * arch_size + 8)[0]
                       for i in range(count)]
            for offset in offsets:
                parse_slice(offset)
            return {"format": "Mach-O universal", "uuids": result}
    return {"format": "not Mach-O", "uuids": [],
            "uuid_note": "LC_UUID is unavailable for this executable format"}


def binary_info(path: Path) -> dict:
    path = path.resolve(strict=True)
    require(path.is_file(), f"Binary is not a file: {path}")
    return {"path": str(path), "bytes": path.stat().st_size,
            "sha256": sha256(path), **macho_uuids(path)}


def numeric(value, label: str, optional: bool = False):
    if value is None and optional:
        return None
    require(isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(value) and value >= 0, f"Invalid metric {label}: {value!r}")
    return value


def summary(values: list) -> dict:
    present = [value for value in values if value is not None]
    return {"runs_1_2_3": values, "available_runs": len(present),
            "median": statistics.median(present) if present else None,
            "min": min(present) if present else None,
            "max": max(present) if present else None}


def paired(before: list, after: list, timing: bool) -> dict:
    first, second = summary(before), summary(after)
    a, b = first["median"], second["median"]
    ratio = a / b if a is not None and b not in (None, 0) else None
    reduction = (a - b) / a * 100 if a not in (None, 0) and b is not None else None
    result = {"before": first, "after": second,
              "median_reduction_percent": reduction}
    result["speedup_before_over_after_median" if timing else
           "before_over_after_median_ratio"] = ratio
    if ratio is None:
        result["ratio_note"] = "Unavailable metric or zero after median; no infinity is reported"
    return result


def cases(raw: dict) -> list[dict]:
    workload = raw.get("workload")
    require(isinstance(workload, dict), "Missing workload")
    result = []
    for case in workload.get("queries", []):
        require(isinstance(case, dict), "Invalid query case")
        require(isinstance(case.get("case"), str) and bool(case["case"]), "Missing query case name")
        require(isinstance(case.get("rows"), int) and not isinstance(case["rows"], bool) and case["rows"] >= 0,
                f"Missing query row count: {case.get('case')}")
        fields = case.get("selected_fields")
        require(isinstance(fields, list) and bool(fields) and all(isinstance(field, str) for field in fields)
                and len(set(fields)) == len(fields), f"Invalid selected fields: {case.get('case')}")
        require(isinstance(case.get("loaded_window_rows"), int) and case["loaded_window_rows"] >= case["rows"],
                f"Invalid loaded window row count: {case.get('case')}")
        require(case.get("correctness", {}).get("all_ids_and_source_rows_checked") == case.get("rows"),
                f"Missing query correctness checks: {case.get('case')}")
        result.append({"kind": "query", "metric": "query_milliseconds", "data": case,
                       "shape": {key: case.get(key) for key in
                                 ("case", "rows", "loaded_window_rows", "selected_fields", "filter", "sort")}})
    exported = list(workload.get("exports", []))
    if "original_snapshot_export" in workload:
        exported.append(workload["original_snapshot_export"])
    for case in exported:
        require(isinstance(case, dict) and isinstance(case.get("case"), str), "Invalid export case")
        require(isinstance(case.get("rows"), int) and not isinstance(case["rows"], bool) and case["rows"] >= 0,
                f"Missing export row count: {case.get('case')}")
        fields = case.get("fields")
        require(isinstance(fields, list) and bool(fields) and all(isinstance(field, str) for field in fields)
                and len(set(fields)) == len(fields), f"Invalid export fields: {case.get('case')}")
        require(case.get("format") in ("parquet", "json", "xlsx", "xls"),
                f"Invalid export format: {case.get('case')}")
        require(case.get("all_exported_id_note_values_and_order_checked") is True,
                f"Missing export correctness checks: {case.get('case')}")
        result.append({"kind": "export", "metric": "export_milliseconds", "data": case,
                       "shape": {key: case.get(key) for key in ("case", "rows", "format", "fields")}})
    require(result, "Workload has no cases")
    keys = [json.dumps({"kind": case["kind"], "shape": case["shape"]}, sort_keys=True)
            for case in result]
    require(len(set(keys)) == len(keys), "Duplicate case identities")
    return result


def compare(raw_directory: Path, before_binary: Path, after_binary: Path) -> dict:
    root = Path(__file__).resolve().parent.parent
    body = root / "examples/check_projection_performance.rs"
    archived_body = root / "target/projection-baseline-20261101/examples/check_projection_performance.rs"
    body_hash, archived_hash = sha256(body), sha256(archived_body)
    require(body_hash == archived_hash, "Before/after benchmark bodies differ")
    binaries = {"before": binary_info(before_binary), "after": binary_info(after_binary)}
    source_stamp = source_path = source_rows = None
    datasets = {}
    inputs = []
    for mode in MODES:
        datasets[mode] = {version: [] for version in VERSIONS}
        reference_shapes = None
        for version in VERSIONS:
            for run in RUNS:
                base = raw_directory / f"{mode}-{version}-{run}"
                raw_path = base.with_suffix(".json")
                system_path = Path(str(base) + "-system.json")
                raw, system = read_json(raw_path), read_json(system_path)
                require(system.get("exit_code") == 0, f"Workload failed: {system_path}")
                require(raw.get("mode") == mode, f"Wrong benchmark mode: {raw_path}")
                require(isinstance(raw.get("source_rows"), int) and raw["source_rows"] >= 2,
                        f"Invalid source row count: {raw_path}")
                require(raw.get("source_metadata_unchanged") is True,
                        f"Source was not verified read-only: {raw_path}")
                stamp = raw.get("source_stamp_before")
                require(isinstance(stamp, dict) and all(key in stamp for key in ("bytes", "device", "inode", "mtime_ns")),
                        f"Missing source stamp: {raw_path}")
                require(stamp == raw.get("source_stamp_after"), f"Source changed during run: {raw_path}")
                canonical_source = str(Path(raw["source"]).resolve())
                require(str(Path(system["source"]).resolve()) == canonical_source,
                        f"Raw/system sources disagree: {base}")
                require(str(Path(system["binary"]).resolve()) == binaries[version]["path"],
                        f"Profile used a different {version} binary: {system_path}")
                if source_stamp is None:
                    source_stamp, source_path, source_rows = stamp, canonical_source, raw.get("source_rows")
                require((stamp, canonical_source, raw.get("source_rows")) ==
                        (source_stamp, source_path, source_rows),
                        f"Runs do not use the same source identity/content stamp: {raw_path}")
                extracted = cases(raw)
                shapes = [{"kind": case["kind"], "shape": case["shape"]} for case in extracted]
                if reference_shapes is None:
                    reference_shapes = shapes
                require(shapes == reference_shapes, f"Case order/rows/fields/filter/sort differ: {raw_path}")
                for case in extracted:
                    numeric(case["data"].get(case["metric"]), f"{base}/{case['shape']['case']}")
                for metric in TIME_METRICS:
                    numeric(system.get(metric), f"{base}/{metric}")
                for metric in MEMORY_METRICS + SPILL_METRICS:
                    numeric(system.get(metric), f"{base}/{metric}", optional=True)
                inputs.append({"mode": mode, "version": version, "run": run,
                               "raw_json": str(raw_path.resolve()), "raw_sha256": sha256(raw_path),
                               "system_json": str(system_path.resolve()), "system_sha256": sha256(system_path)})
                datasets[mode][version].append({"raw": raw, "system": system, "cases": extracted})
    workloads = {}
    for mode in MODES:
        versions = datasets[mode]
        compared = []
        for index, first in enumerate(versions["before"][0]["cases"]):
            metric = first["metric"]
            values = {version: [record["cases"][index]["data"][metric] for record in versions[version]]
                      for version in VERSIONS}
            item = {"kind": first["kind"], "shape": first["shape"], "metric": metric,
                    "measurement": paired(values["before"], values["after"], timing=True)}
            item["validation_milliseconds"] = {
                version: summary([numeric(record["cases"][index]["data"].get("validation_milliseconds"),
                                          "validation_milliseconds", optional=True)
                                  for record in versions[version]]) for version in VERSIONS}
            if first["kind"] == "query":
                item["window_cache_hit_facts"] = {
                    version: [record["cases"][index]["data"].get("timings", {}).get("window_cache_hit")
                              for record in versions[version]] for version in VERSIONS}
                item["field_buffer_sharing_facts"] = {
                    version: [record["cases"][index]["data"].get("buffer_sharing_with_original")
                              for record in versions[version]] for version in VERSIONS}
                item["engine_stage_milliseconds"] = {}
                for stage in ("window_load_ms", "index_query_ms", "metadata_ms", "arrow_start_ms", "arrow_fetch_ms", "adapter_ms"):
                    values = {version: [numeric(record["cases"][index]["data"].get("timings", {}).get(stage),
                                                stage, optional=True) for record in versions[version]]
                              for version in VERSIONS}
                    item["engine_stage_milliseconds"][stage] = paired(values["before"], values["after"], timing=True)
            else:
                item["output_bytes"] = {version: [record["cases"][index]["data"].get("output_bytes")
                                                   for record in versions[version]] for version in VERSIONS}
            compared.append(item)
        system_groups = {}
        for group, metrics, timing in (("whole_workload_time", TIME_METRICS, True),
                                       ("whole_workload_memory", MEMORY_METRICS, False),
                                       ("whole_workload_spill", SPILL_METRICS, False)):
            system_groups[group] = {metric: paired(
                [record["system"].get(metric) for record in versions["before"]],
                [record["system"].get(metric) for record in versions["after"]], timing=timing)
                for metric in metrics}
        workloads[mode] = {"cases": compared, **system_groups,
                           "open_milliseconds": paired(
                               [numeric(record["raw"].get("open_milliseconds"), "open_milliseconds") for record in versions["before"]],
                               [numeric(record["raw"].get("open_milliseconds"), "open_milliseconds") for record in versions["after"]], timing=True),
                           "environment_mode_facts": {version: [record["raw"].get("environment_mode")
                                                                 for record in versions[version]] for version in VERSIONS}}
    return {"protocol": "Three runs per engine and mode, identical case order and source stamp. Ratios use before median / after median; below 1 means slower, above 1 faster. All three values and min/max are retained.",
            "source": source_path, "source_rows": source_rows, "source_stamp": source_stamp,
            "binaries": binaries, "same_binary_sha256": binaries["before"]["sha256"] == binaries["after"]["sha256"],
            "benchmark_body": {"current_path": str(body), "current_sha256": body_hash,
                               "archived_path": str(archived_body), "archived_sha256": archived_hash, "identical": True},
            "input_reports": inputs, "workloads": workloads,
            "memory_interpretation": "All RSS/physical-footprint/spill figures describe the entire launched workload, including loading, sorting, validation, retained snapshots and teardown. The import peak can dominate export mode. No export-stage peak-memory measurement or projection-only memory reduction is inferred.",
            "buffer_interpretation": "Reported sharing is observed Arrow data/offset-buffer address overlap with the retained original snapshot. It is not RSS, a requirement imposed on the baseline, or proof that a particular number of source columns was read.",
            "limits": "OS page-cache state and run order are controlled externally. Source stamps verify identity/size/mtime, not cryptographic content equality; hash the shared source separately when required. Binary hashes/UUIDs identify the supplied artifacts; build provenance still comes from the recorded build procedure."}


def main() -> None:
    if len(sys.argv) != 5:
        raise SystemExit(__doc__.strip())
    raw, before, after, output = map(Path, sys.argv[1:])
    require(not output.exists() and not output.is_symlink(), "Choose a new output path; existing files are never overwritten")
    report = compare(raw, before, after)
    text = json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    with output.open("x") as stream:
        stream.write(text)
    print(json.dumps({"output": str(output.resolve()), "modes": list(MODES), "runs_per_version": 3,
                      "validated_input_reports": len(report["input_reports"]), "same_body": True}))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, json.JSONDecodeError) as error:
        print(f"Projection comparison failed: {error}", file=sys.stderr)
        raise SystemExit(1)
