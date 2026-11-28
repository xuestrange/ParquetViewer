"""Compare optimized check_csv_import binaries on identical, physically written CSVs.

    python3 examples/benchmark_csv_index.py before=/path/to/before after=/path/to/after

Build each binary with the same release flags and the index/page fixture modes in
check_csv_import. Each scan starts a fresh process. Files are read sequentially
before measurements to compare warm-cache parser costs. All fixtures are removed
on exit. Standard output is a JSON report with every individual measurement.
"""

import argparse
import json
import math
import os
from pathlib import Path
from statistics import median
import subprocess
import sys
import tempfile
import time


def write_fixture(path, profile):
    wide_columns = 96
    counts = {
        "short": 3_000_000,
        "multiline": 1_500_000,
        "empty_fields": 650_000,
        "wide_text": 1_012,
        "wrong_width": 50_000,
    }
    rows = counts[profile]
    with path.open("xb", buffering=1024 * 1024) as output:
        if profile == "empty_fields":
            header = b"id," + b",".join(f"c{i}".encode() for i in range(wide_columns))
            output.write(header + b"\n")
            suffix = b"," * wide_columns + b"\n"
        elif profile == "short":
            output.write(b"id,amount,code,flag\n")
            suffix = b",123456.78,SWIFT,true\n"
        elif profile == "multiline":
            output.write(b"id,description,code\r\n")
            suffix = b',"Central, ""alpha""\r\nsecond line",X1\r\n'
        else:
            output.write(b"id,payload\n")
            suffix = b",first\n"
        for row in range(rows):
            if profile == "wide_text" and row == 1_000:
                suffix = b',"' + b"abcdefgh" * (12 * 1024 * 1024 // 8) + b'\nend"\n'
            output.write(str(row).encode())
            output.write(suffix)
        valid_bytes = output.tell()
        if profile == "wrong_width":
            # A two-column source with one late 8 MiB row of empty fields.
            # Accumulating each field's end offset is expensive even though
            # there is no cell payload to copy.
            output.write(b"," * (8 * 1024 * 1024) + b"\n")
        output.flush()
        os.fsync(output.fileno())
    return rows, valid_bytes


def sequential_read(path):
    block = bytearray(256 * 1024)
    started = time.perf_counter()
    with path.open("rb", buffering=0) as source:
        while source.readinto(block):
            pass
    return 1000 * (time.perf_counter() - started)


def run(binary, mode, path):
    # wait4 supplies per-child CPU time and the kernel-recorded peak RSS,
    # including short-lived allocations missed by interval sampling.
    with tempfile.TemporaryFile() as output:
        with subprocess.Popen([binary, mode, path], stdout=output) as process:
            _, status, usage = os.wait4(process.pid, 0)
            process.returncode = os.waitstatus_to_exitcode(status)
            if process.returncode:
                raise subprocess.CalledProcessError(process.returncode, process.args)
        output.seek(0)
        measurement = json.load(output)
    rss_bytes = usage.ru_maxrss if sys.platform == "darwin" else usage.ru_maxrss * 1024
    measurement["peak_rss_mib"] = rss_bytes / 1024**2
    measurement["process_cpu_ms"] = 1000 * (usage.ru_utime + usage.ru_stime)
    return measurement


def check(result, profile, rows, valid_bytes, file_size):
    if result["indexed_rows"] != rows:
        raise ValueError(f"{profile}: unexpected row count {result}")
    if profile == "wrong_width":
        if result["complete"] or f"record {rows + 1} has 8388609 fields" not in (result["error"] or ""):
            raise ValueError(f"{profile}: missing expected width diagnostic {result}")
        if result["indexed_bytes"] != valid_bytes:
            raise ValueError(f"{profile}: wrong verified prefix {result}")
    elif result["error"] or not result["complete"] or result["indexed_bytes"] != file_size:
        raise ValueError(f"{profile}: incomplete indexing {result}")


def benchmark(binaries, directory, repeats):
    report = {"cache": "warm", "repeats": repeats, "binaries": dict(binaries), "profiles": {}}
    for profile in ("short", "multiline", "empty_fields", "wide_text", "wrong_width"):
        path = directory / f"{profile}.csv"
        rows, valid_bytes = write_fixture(path, profile)
        stat = path.stat()
        sequential_read(path)
        raw_ms = [sequential_read(path) for _ in range(repeats)]
        trials = {name: [] for name, _ in binaries}
        for trial in range(repeats):
            # Rotate the order to spread hardware/cache variation across builds.
            offset = trial % len(binaries)
            for name, binary in binaries[offset:] + binaries[:offset]:
                result = run(binary, "--index-fixture", path)
                check(result, profile, rows, valid_bytes, stat.st_size)
                trials[name].append(result)
        builds = {}
        for name, binary in binaries:
            pages = run(binary, "--page-fixture", path)
            check(pages, profile, rows, valid_bytes, stat.st_size)
            jumps = sorted(pages["jump_ms"])
            scans = trials[name]
            builds[name] = {
                "index_median_ms": median(run["index_ms"] for run in scans),
                "peak_rss_median_mib": median(run["peak_rss_mib"] for run in scans),
                "first_typed_page_ms": pages["first_typed_page_ms"],
                "jump_p95_ms": jumps[math.ceil(0.95 * len(jumps)) - 1],
                "runs": scans,
            }
        report["profiles"][profile] = {
            "rows": rows, "source_bytes": stat.st_size, "allocated_bytes": stat.st_blocks * 512,
            "sequential_read_median_ms": median(raw_ms), "builds": builds,
        }
        path.unlink()
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="+", metavar="NAME=PATH")
    parser.add_argument("--repeats", type=int, default=5)
    args = parser.parse_args()
    binaries = []
    for item in args.binary:
        name, separator, path = item.partition("=")
        if not separator or not name or any(name == old for old, _ in binaries):
            parser.error("use a unique NAME=PATH for each binary")
        binary = Path(path).resolve()
        if not binary.is_file() or not os.access(binary, os.X_OK):
            parser.error(f"missing executable {binary}")
        binaries.append((name, str(binary)))
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    with tempfile.TemporaryDirectory(prefix="csv-index-benchmark-") as directory:
        report = benchmark(binaries, Path(directory), args.repeats)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
