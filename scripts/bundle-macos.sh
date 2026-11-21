#!/usr/bin/env bash
# Build a local, unsigned-for-distribution macOS application bundle.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/bundle-macos.sh [--debug] [--no-sign] [--skip-build] [--archive] [--dmg]

Build with locked dependencies and create dist/Parquet Viewer.app.
The default is a release build. --debug uses the debug build and its cache.
The default ad hoc signature is for local use, not notarized distribution.
Use --no-sign to omit the ad hoc signature. Nothing is installed.
Use --skip-build to package an existing binary after a separately verified build.
Use --archive to also create a compressed ZIP for transferring the application.
The ZIP reduces transfer size; the extracted .app still has its own disk size.
Use --dmg to create a compressed disk image with an Applications shortcut.

CARGO_BUILD_TARGET may select an installed Apple target; the default is the
current Rust host. CARGO_TARGET_DIR is respected.
PARQUET_VIEWER_VERSION and PARQUET_VIEWER_BUILD_NUMBER override the bundle
version metadata. When using --skip-build, set the version to match the binary.
EOF
}

ad_hoc_sign=true
build_profile=release
skip_build=false
create_archive=false
create_dmg=false
for argument in "$@"; do
    case "$argument" in
        --debug) build_profile=debug ;;
        --no-sign) ad_hoc_sign=false ;;
        --skip-build) skip_build=true ;;
        --archive) create_archive=true ;;
        --dmg) create_dmg=true ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

if [[ "$(uname -s)" != Darwin ]]; then
    echo "This script must run on macOS." >&2
    exit 1
fi

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if [[ -n "${PARQUET_VIEWER_VERSION:-}" && ! "$PARQUET_VIEWER_VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "PARQUET_VIEWER_VERSION must be a version such as 1.0.0." >&2
    exit 2
fi
if [[ -n "${PARQUET_VIEWER_BUILD_NUMBER:-}" && ! "$PARQUET_VIEWER_BUILD_NUMBER" =~ ^[1-9][0-9]*$ ]]; then
    echo "PARQUET_VIEWER_BUILD_NUMBER must be a positive integer." >&2
    exit 2
fi

host_target="$(rustc -vV | awk '/^host:/ {print $2}')"
case "${CARGO_BUILD_TARGET:-$host_target}" in
    aarch64-apple-darwin|x86_64-apple-darwin) ;;
    *) echo "Unsupported macOS target: ${CARGO_BUILD_TARGET:-$host_target}" >&2; exit 1 ;;
esac

project_target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
if [[ "$project_target_dir" != /* ]]; then
    project_target_dir="$project_root/$project_target_dir"
fi

# Rust embeds the application PNG at compile time.
"$project_root/scripts/build-icons.sh" --if-needed

build_arguments=(build --locked --bin parquet-viewer --target-dir "$project_target_dir")
if [[ "$build_profile" == release ]]; then
    build_arguments+=(--release)
fi
binary_directory="$project_target_dir/$build_profile"
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
    build_arguments+=(--target "$CARGO_BUILD_TARGET")
    binary_directory="$project_target_dir/$CARGO_BUILD_TARGET/$build_profile"
fi
if [[ "$skip_build" == false ]]; then
    cargo "${build_arguments[@]}"
elif [[ ! -x "$binary_directory/parquet-viewer" ]]; then
    echo "No executable found for --skip-build: $binary_directory/parquet-viewer" >&2
    exit 1
fi

app_bundle="$project_root/dist/Parquet Viewer.app"
bundle_contents="$app_bundle/Contents"
mkdir -p "$bundle_contents/MacOS" "$bundle_contents/Resources"
install -m 755 "$binary_directory/parquet-viewer" \
    "$bundle_contents/MacOS/parquet-viewer"
install -m 644 "$project_root/packaging/Info.plist" "$bundle_contents/Info.plist"
for locale in en zh-Hans; do
    mkdir -p "$bundle_contents/Resources/$locale.lproj"
    install -m 644 "$project_root/packaging/$locale.lproj/InfoPlist.strings" \
        "$bundle_contents/Resources/$locale.lproj/InfoPlist.strings"
done
if [[ -n "${PARQUET_VIEWER_VERSION:-}" ]]; then
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $PARQUET_VIEWER_VERSION" \
        "$bundle_contents/Info.plist"
fi
if [[ -n "${PARQUET_VIEWER_BUILD_NUMBER:-}" ]]; then
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $PARQUET_VIEWER_BUILD_NUMBER" \
        "$bundle_contents/Info.plist"
fi
install -m 644 "$project_root/packaging/assets/"{ParquetViewer,CSVDocument,ParquetDocument}.icns \
    "$bundle_contents/Resources/"
install -m 644 "$project_root/LICENSE" "$bundle_contents/Resources/LICENSE"
install -m 644 "$project_root/THIRD_PARTY_NOTICES.md" \
    "$bundle_contents/Resources/THIRD_PARTY_NOTICES.md"
printf 'APPL????' > "$bundle_contents/PkgInfo"
plutil -lint "$bundle_contents/Info.plist"

if [[ "$ad_hoc_sign" == true ]]; then
    codesign --force --sign - "$app_bundle"
    codesign --verify --deep --strict "$app_bundle"
fi

# Updating Contents alone leaves an existing .app directory's modification
# time unchanged. Mark the completed bundle so Finder can refresh its metadata.
touch "$app_bundle"

size_arguments=("$project_root/scripts/inspect-package-size.py" "$app_bundle")
if [[ "$create_archive" == true || "$create_dmg" == true ]]; then
    case "${CARGO_BUILD_TARGET:-$host_target}" in
        aarch64-apple-darwin) artifact_suffix=arm64 ;;
        x86_64-apple-darwin) artifact_suffix=x86_64 ;;
    esac
    if [[ "$build_profile" == debug ]]; then
        artifact_suffix+="-debug"
    fi
fi
if [[ "$create_archive" == true ]]; then
    archive_path="$project_root/dist/Parquet Viewer-macOS-$artifact_suffix.zip"
    archive_temporary="$(mktemp "$project_root/dist/.parquet-viewer-archive.XXXXXX")"
    trap 'rm -f "$archive_temporary"' EXIT
    /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$app_bundle" "$archive_temporary"
    /usr/bin/unzip -tq "$archive_temporary"
    mv -f "$archive_temporary" "$archive_path"
    trap - EXIT
    size_arguments+=(--archive "$archive_path")
    echo "Archive: $archive_path"
fi
if [[ "$create_dmg" == true ]]; then
    dmg_path="$project_root/dist/Parquet-Viewer-macOS-$artifact_suffix.dmg"
    dmg_temporary="$(mktemp -d "$project_root/dist/.parquet-viewer-dmg.XXXXXX")"
    trap 'rm -rf "$dmg_temporary"' EXIT
    mkdir "$dmg_temporary/content"
    /usr/bin/ditto "$app_bundle" "$dmg_temporary/content/Parquet Viewer.app"
    ln -s /Applications "$dmg_temporary/content/Applications"
    # Size the volume from logical bytes and leave room for filesystem metadata.
    source_kib="$(du -Ask "$dmg_temporary/content" | cut -f 1)"
    image_mib=$(( (source_kib + 1023) / 1024 + 32 ))
    if ! /usr/bin/hdiutil create -fs HFS+ -size "${image_mib}m" \
        -format UDZO -volname "Parquet Viewer" -srcfolder "$dmg_temporary/content" \
        "$dmg_temporary/Parquet Viewer.dmg"; then
        df -h "$dmg_temporary" "${TMPDIR:-/tmp}" >&2
        exit 1
    fi
    /usr/bin/hdiutil verify "$dmg_temporary/Parquet Viewer.dmg"
    mv -f "$dmg_temporary/Parquet Viewer.dmg" "$dmg_path"
    rm -rf "$dmg_temporary"
    trap - EXIT
    echo "Disk image: $dmg_path"
fi
python3 "${size_arguments[@]}"

echo "Created: $app_bundle"
echo "Launch: open \"$app_bundle\""
echo "Open a file: open -a \"$app_bundle\" /absolute/path/to/data.parquet"
