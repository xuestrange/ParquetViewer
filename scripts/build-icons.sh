#!/usr/bin/env bash
# Rebuild the application and document artwork into full macOS ICNS sets.
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
    echo "Icon generation uses macOS AppKit, sips and iconutil." >&2
    exit 1
fi

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
icon_assets="$project_root/packaging/assets"
icon_work_dir="$(mktemp -d "${TMPDIR:-/tmp}/parquetviewer-icons.XXXXXX")"
trap 'rm -rf "$icon_work_dir"' EXIT

renderer="$project_root/scripts/render-icon.swift"
swift_cache="$project_root/target/icon-tools/swift-cache"
mkdir -p "$swift_cache"
render=(swift -module-cache-path "$swift_cache" "$renderer")

if_needed=false
if [[ "${1:-}" == "--if-needed" ]]; then
    if_needed=true
elif [[ $# -gt 0 ]]; then
    echo "Usage: scripts/build-icons.sh [--if-needed]" >&2
    exit 2
fi

build_icon() {
    local source="$icon_assets/$1.svg" bundle="$icon_assets/$2.icns"
    local raster="${3:-$icon_work_dir/$2.png}" iconset="$icon_work_dir/$2.iconset"
    if [[ "$if_needed" == true && -s "$bundle" && ! "$source" -nt "$bundle" &&
          ! "$renderer" -nt "$bundle" && ! "${BASH_SOURCE[0]}" -nt "$bundle" &&
          ( $# -lt 3 || ( -s "$raster" && ! "$source" -nt "$raster" ) ) ]]; then
        return
    fi

    "${render[@]}" "$source" "$raster" 1024
    mkdir -p "$iconset"
    local points pixels
    for points in 16 32 128 256 512; do
        sips -z "$points" "$points" "$raster" --out "$iconset/icon_${points}x${points}.png" >/dev/null
        pixels=$((points * 2))
        sips -z "$pixels" "$pixels" "$raster" --out "$iconset/icon_${points}x${points}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$bundle"
    echo "Created: $bundle"
}

build_icon app-icon ParquetViewer "$icon_assets/app-icon-1024.png"
build_icon csv-document CSVDocument
build_icon parquet-document ParquetDocument

preview="$icon_assets/app-icon-preview.png"
if [[ "$if_needed" == false || ! -s "$preview" || "$icon_assets/ParquetViewer.icns" -nt "$preview" ||
      "$icon_assets/CSVDocument.icns" -nt "$preview" || "$icon_assets/ParquetDocument.icns" -nt "$preview" ]]; then
    "${render[@]}" "$icon_assets/app-icon.svg" "$preview" 224 \
        "$icon_assets/csv-document.svg" "$icon_assets/parquet-document.svg"
fi
