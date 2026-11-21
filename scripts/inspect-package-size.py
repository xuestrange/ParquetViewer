#!/usr/bin/env python3
"""Measure the actual app, executable, and optional compressed distribution.

Usage: inspect-package-size.py APP [--archive ZIP] [--baseline-binary FILE]
                               [--output NEW_JSON]
The baseline option compares executables only; it does not invent an old app or
archive size. Logical bytes and allocated blocks are separate measurements.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import plistlib
import struct
import uuid
import zipfile


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def macho(path: Path) -> dict:
    """File-backed segments only: __PAGEZERO is not four GB on disk."""
    with path.open("rb") as stream:
        header = stream.read(32)
        if header[:4] not in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
            return {"format": "not a thin 64-bit Mach-O", "segments": []}
        endian = "<" if header[:4] == b"\xcf\xfa\xed\xfe" else ">"
        cpu, _, _, command_count, command_bytes = struct.unpack_from(endian + "IIIII", header, 4)
        if command_bytes > path.stat().st_size - 32 or command_count > 65536:
            raise ValueError(f"Invalid Mach-O command table: {path}")
        commands = stream.read(command_bytes)
    position = 0
    segments, identities = [], []
    for _ in range(command_count):
        command, size = struct.unpack_from(endian + "II", commands, position)
        if size < 8 or position + size > len(commands):
            raise ValueError(f"Invalid Mach-O load command: {path}")
        if command == 0x1B:
            identities.append(str(uuid.UUID(bytes=commands[position + 8:position + 24])).upper())
        elif command == 0x19:
            segment = commands[position:position + size]
            name = segment[8:24].split(b"\0", 1)[0].decode("ascii")
            file_offset, file_bytes = struct.unpack_from(endian + "QQ", segment, 40)
            section_count = struct.unpack_from(endian + "I", segment, 64)[0]
            sections = []
            for index in range(section_count):
                offset = 72 + index * 80
                section_name = segment[offset:offset + 16].split(b"\0", 1)[0].decode("ascii")
                address_bytes = struct.unpack_from(endian + "Q", segment, offset + 40)[0]
                flags = struct.unpack_from(endian + "I", segment, offset + 64)[0]
                sections.append({"name": section_name, "address_bytes": address_bytes,
                                 "zero_fill": flags & 0xFF in (1, 12, 18)})
            segments.append({"name": name, "file_offset": file_offset,
                             "file_bytes": file_bytes, "sections": sections})
        position += size
    return {"format": "Mach-O 64-bit", "architecture": {0x0100000C: "arm64", 0x01000007: "x86_64"}.get(cpu, str(cpu)),
            "uuids": identities, "segments": segments,
            "scope": "Segment file_bytes are file-backed ranges. Section address_bytes can include zero-fill and are not added to package disk usage."}


def file_info(path: Path) -> dict:
    stat = path.stat()
    return {"path": str(path.resolve()), "logical_bytes": stat.st_size,
            "allocated_bytes": stat.st_blocks * 512, "sha256": sha256(path)}


def inspect(app: Path, archive: Path | None, baseline: Path | None) -> dict:
    app = app.resolve(strict=True)
    if not app.is_dir() or app.suffix != ".app":
        raise ValueError("Provide a macOS .app directory")
    contents = app / "Contents"
    info = plistlib.loads((contents / "Info.plist").read_bytes())
    binary = contents / "MacOS" / info["CFBundleExecutable"]
    binary_record = {**file_info(binary), "mach_o": macho(binary)}
    files = sorted(path for path in app.rglob("*") if path.is_file() and not path.is_symlink())
    inventory = []
    for path in files:
        stat = path.stat()
        inventory.append({"relative_path": str(path.relative_to(app)),
                          "logical_bytes": stat.st_size, "allocated_bytes": stat.st_blocks * 512})
    total = sum(item["logical_bytes"] for item in inventory)
    result = {
        "measured_utc": datetime.now(timezone.utc).isoformat(),
        "app": {"path": str(app), "logical_file_bytes": total,
                "allocated_file_bytes": sum(item["allocated_bytes"] for item in inventory),
                "regular_file_count": len(inventory), "files": inventory},
        "executable": binary_record,
        "limits": "Logical sizes count regular file data; allocation uses st_blocks and can depend on APFS/compression/cloning. Directory metadata, symlink payloads, resource forks and filesystem sharing are not modeled as occupied physical storage. ZIP size is a transfer artifact, not the extracted app size.",
    }
    if archive is not None:
        archive_record = file_info(archive)
        with zipfile.ZipFile(archive) as zipped:
            bad = zipped.testzip()
            if bad is not None:
                raise ValueError(f"ZIP CRC check failed: {bad}")
            entries = zipped.infolist()
            executable_entry = f"{app.name}/Contents/MacOS/{info['CFBundleExecutable']}"
            with zipped.open(executable_entry) as executable:
                archived_hash = hashlib.file_digest(executable, "sha256").hexdigest()
            if archived_hash != binary_record["sha256"]:
                raise ValueError("Archive contains a different executable than the inspected app")
            if zipped.read(f"{app.name}/Contents/Info.plist") != (contents / "Info.plist").read_bytes():
                raise ValueError("Archive Info.plist differs from the inspected app")
            archive_record.update(entries=len(entries), crc_check_passed=True,
                                  executable_and_info_match_app=True,
                                  expanded_entry_bytes=sum(item.file_size for item in entries),
                                  payload_compressed_bytes=sum(item.compress_size for item in entries))
        archive_record["archive_to_app_logical_ratio"] = archive_record["logical_bytes"] / total if total else None
        result["archive"] = archive_record
    if baseline is not None:
        before = {**file_info(baseline), "mach_o": macho(baseline)}
        first, second = before["logical_bytes"], binary_record["logical_bytes"]
        result["baseline_executable"] = before
        result["executable_comparison"] = {"before_bytes": first, "after_bytes": second,
                                           "reduction_bytes": first - second,
                                           "reduction_percent": (first - second) / first * 100 if first else None,
                                           "scope": "Executable file bytes only; baseline .app and archive sizes are not inferred."}
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", type=Path)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--baseline-binary", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    report = inspect(args.app, args.archive, args.baseline_binary)
    text = json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    if args.output is not None:
        with args.output.open("x") as stream:
            stream.write(text)
        print(json.dumps({"report": str(args.output.resolve()),
                          "executable_bytes": report["executable"]["logical_bytes"],
                          "app_logical_file_bytes": report["app"]["logical_file_bytes"],
                          "archive_bytes": report.get("archive", {}).get("logical_bytes")}))
    else:
        print(text, end="")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, struct.error, zipfile.BadZipFile) as error:
        raise SystemExit(f"Package inspection failed: {error}") from error
