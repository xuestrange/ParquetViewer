#!/usr/bin/env python3
"""Profile a standalone migration or load-verification binary on macOS.

Usage: python3 scripts/profile-load.py BINARY SOURCE_PARQUET OUTPUT_PREFIX
The prefix creates PREFIX.json and PREFIX-system.json; use a fresh path.
20 ms samples estimate the peak; spill extent is not cumulative disk writes.
"""
from pathlib import Path
import ctypes
import json
import resource
import subprocess
import sys
import tempfile
import time


class RusageInfoV0(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_ubyte * 16)] + [
        (name, ctypes.c_uint64)
        for name in (
            "user_time", "system_time", "package_idle_wakeups", "interrupt_wakeups",
            "pageins", "wired_size", "resident_size", "physical_footprint",
            "process_start_abstime", "process_exit_abstime",
        )
    ]


if sys.platform != "darwin" or len(sys.argv) != 4:
    raise SystemExit("Usage on macOS: profile-load.py BINARY SOURCE_PARQUET FRESH_OUTPUT_PREFIX")
binary, source, prefix = sys.argv[1:]
if any(Path(prefix + suffix).exists() for suffix in (".json", "-system.json")):
    raise SystemExit("Choose a fresh output prefix; existing reports are not overwritten.")
lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
lib.proc_pid_rusage.restype = ctypes.c_int
started = time.monotonic()
samples = []
with Path(prefix + ".json").open("x") as output:
    child = subprocess.Popen([binary, source], stdout=output, stderr=subprocess.PIPE, text=True)
    try:
        while child.poll() is None:
            info = RusageInfoV0()
            status = lib.proc_pid_rusage(child.pid, 0, ctypes.byref(info))
            disk_bytes = allocated_bytes = 0
            for folder in Path(tempfile.gettempdir()).glob(f"parquet-viewer-engine-{child.pid}-*"):
                for item in folder.rglob("*"):
                    try:
                        if item.is_file():
                            stat = item.stat()
                            disk_bytes += stat.st_size
                            allocated_bytes += stat.st_blocks * 512
                    except FileNotFoundError:
                        pass
            samples.append({
                "seconds": round(time.monotonic() - started, 3),
                "procinfo_status": status,
                "rss_bytes": info.resident_size if status == 0 else None,
                "physical_footprint_bytes": info.physical_footprint if status == 0 else None,
                "spill_logical_bytes": disk_bytes,
                "spill_allocated_bytes": allocated_bytes,
            })
            time.sleep(0.02)
        error = child.communicate()[1]
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait()
usage = resource.getrusage(resource.RUSAGE_CHILDREN)
success = [item for item in samples if item["procinfo_status"] == 0]
statistics = {
    "binary": str(Path(binary).resolve()),
    "source": str(Path(source).resolve()),
    "exit_code": child.returncode,
    "wall_seconds": time.monotonic() - started,
    "child_cpu_seconds": usage.ru_utime + usage.ru_stime,
    "child_peak_rss_bytes_macos": usage.ru_maxrss,
    "max_sampled_rss_bytes": max((item["rss_bytes"] for item in success), default=None),
    "max_sampled_physical_footprint_bytes": max((item["physical_footprint_bytes"] for item in success), default=None),
    "max_sampled_spill_logical_bytes": max((item["spill_logical_bytes"] for item in samples), default=0),
    "max_sampled_spill_allocated_bytes": max((item["spill_allocated_bytes"] for item in samples), default=0),
    "sampling_interval_ms": 20,
    "successful_procinfo_samples": len(success),
    "scope": "Whole launched CLI workload, including engine startup, queries and teardown. Companion JSON describes its operation sequence and query-only timings. RSS and physical footprint are different OS accounting metrics; Arrow logical buffer bytes are not either one.",
    "stderr": error,
    "samples": samples,
}
with Path(prefix + "-system.json").open("x") as output:
    json.dump(statistics, output, indent=2)
print(json.dumps({key: value for key, value in statistics.items() if key != "samples"}, indent=2), flush=True)
raise SystemExit(child.returncode)
