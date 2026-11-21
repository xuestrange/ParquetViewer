#!/usr/bin/env python3
"""Summarize three alternating before/after trials produced by profile-load.py.

Usage: compare-migration.py PREFIX NEW_REPORT.json
Inputs: PREFIX-{small,load-large}-{1,2,3}-{before,after}{,-system}.json
The report includes every trial; a median never replaces the raw observations.
"""
from pathlib import Path
import datetime
import hashlib
import json
import statistics
import subprocess
import sys


def summary(values):
    return {"median": statistics.median(values), "min": min(values),
            "max": max(values), "trials": values}


def binary_identity(path):
    path = Path(path)
    result = subprocess.run(["dwarfdump", "--uuid", str(path)],
                            text=True, capture_output=True, check=True)
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(4 * 1024 * 1024), b""):
            digest.update(chunk)
    return {"path": str(path.resolve()), "bytes": path.stat().st_size,
            "sha256": digest.hexdigest(), "mach_o_uuid": result.stdout.strip()}


if len(sys.argv) != 3:
    raise SystemExit(__doc__)
prefix, output = sys.argv[1:]
if Path(output).exists():
    raise SystemExit("Choose a new report path; existing evidence is not overwritten.")
report = {"generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
          "trials_per_variant": 3, "groups": {}, "binaries": {},
          "scope": "Same-machine serial release trials, alternating before/after order. "
                   "No OS cache flush or exclusive-machine control. Operation time is query wall "
                   "time; OS metrics cover the entire launched verification workload. "
                   "Sampled spill extent is not cumulative disk writes. RSS, physical footprint "
                   "and logical Arrow buffer bytes are distinct metrics."}
metrics = ["wall_seconds", "child_cpu_seconds", "child_peak_rss_bytes_macos",
           "max_sampled_rss_bytes", "max_sampled_physical_footprint_bytes",
           "max_sampled_spill_logical_bytes", "max_sampled_spill_allocated_bytes"]
for group in ("small", "load-large"):
    variants = {}
    identities = set()
    for variant in ("before", "after"):
        operations, systems, paths = [], [], []
        for trial in (1, 2, 3):
            stem = f"{prefix}-{group}-{trial}-{variant}"
            operation = json.loads(Path(stem + ".json").read_text())
            system = json.loads(Path(stem + "-system.json").read_text())
            assert system["exit_code"] == 0, (stem, system["stderr"])
            assert operation["source_size_mtime_unchanged"], stem
            identities.add((operation["source"], operation.get("source_rows", operation.get("all_rows"))))
            operations.append(operation)
            systems.append({key: value for key, value in system.items() if key != "samples"})
            paths.append(stem)
            name = Path(system["binary"]).name
            if name not in report["binaries"]:
                report["binaries"][name] = binary_identity(system["binary"])
        if group == "small":
            assert all([run["case"] for run in item["runs"]] ==
                       [run["case"] for run in operations[0]["runs"]] for item in operations)
            timing = {run["case"]: summary([item["runs"][index]["milliseconds"] for item in operations])
                      for index, run in enumerate(operations[0]["runs"])}
        else:
            timing = {"load_all": summary([item["load_all_ms"] for item in operations])}
        variants[variant] = {"operation_ms": timing,
                             "whole_workload": {metric: summary([item[metric] for item in systems])
                                                for metric in metrics},
                             "raw_prefixes": paths, "operations": operations, "systems": systems}
    assert len(identities) == 1, (group, identities)
    if group == "small":
        for before, after in zip(variants["before"]["operations"], variants["after"]["operations"]):
            assert [(r["case"], r["rows"], r["first_id"], r["last_id"]) for r in before["runs"]] == \
                   [(r["case"], r["rows"], r["first_id"], r["last_id"]) for r in after["runs"]]
    comparison = {}
    for case, old in variants["before"]["operation_ms"].items():
        new = variants["after"]["operation_ms"][case]
        comparison[case] = {"before_median_ms": old["median"], "after_median_ms": new["median"],
                            "speedup": old["median"] / new["median"],
                            "time_reduction_percent": (1 - new["median"] / old["median"]) * 100}
    report["groups"][group] = {"source": next(iter(identities))[0],
                                "source_rows": next(iter(identities))[1],
                                "variants": variants, "comparison": comparison}
with Path(output).open("x") as stream:
    json.dump(report, stream, ensure_ascii=False, indent=2)
print(json.dumps({group: data["comparison"] for group, data in report["groups"].items()}, indent=2))
