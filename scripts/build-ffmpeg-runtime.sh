#!/usr/bin/env bash
# Build the same LGPL FFmpeg 9 source for macOS CI and distributable app bundles.
set -euo pipefail
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
prefix=${1:?Usage: build-ffmpeg-runtime.sh ABSOLUTE_INSTALL_PREFIX}
[[ "$prefix" = /* ]] || { echo "Install prefix must be absolute" >&2; exit 2; }
if [[ -n "${GITHUB_ENV:-}" ]]; then
    printf 'FFMPEG_PREFIX=%s\nPKG_CONFIG_PATH=%s/lib/pkgconfig\nLIBRARY_PATH=%s/lib\nDYLD_LIBRARY_PATH=%s/lib\n' \
        "$prefix" "$prefix" "$prefix" "$prefix" >> "$GITHUB_ENV"
fi
if [[ -f "$prefix/lib/pkgconfig/libavcodec.pc" ]] &&
    cmp -s "$repo_root/vendor/ffmpeg.lock.json" "$prefix/share/lumina-ffmpeg/ffmpeg.lock.json" &&
    PKG_CONFIG_PATH="$prefix/lib/pkgconfig" pkg-config --atleast-version=63 libavcodec; then
    echo "Using verified cached FFmpeg runtime: $prefix"
    exit 0
fi
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
metadata=$(python3 - "$repo_root/vendor/ffmpeg.lock.json" <<'PY_LOCK'
import json, sys
lock = json.load(open(sys.argv[1]))
print(lock["url"])
print(lock["sha256"])
PY_LOCK
)
url=$(printf '%s\n' "$metadata" | sed -n '1p')
sha=$(printf '%s\n' "$metadata" | sed -n '2p')
curl -fL --retry 3 "$url" -o "$work_dir/source.tar.xz"
python3 - "$work_dir/source.tar.xz" "$sha" <<'PY_HASH'
import hashlib, pathlib, sys
assert hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest() == sys.argv[2], "FFmpeg checksum mismatch"
PY_HASH
mkdir "$work_dir/source"
tar -xJf "$work_dir/source.tar.xz" --strip-components=1 -C "$work_dir/source"
cd "$work_dir/source"
platform_options=()
if [[ "$(uname -s)" == Darwin ]]; then
    export SDKROOT=$(xcrun --sdk macosx --show-sdk-path)
    platform_options=(--enable-videotoolbox --enable-audiotoolbox --enable-securetransport)
    jobs=$(sysctl -n hw.ncpu)
else
    jobs=$(getconf _NPROCESSORS_ONLN)
fi
./configure --prefix="$prefix" --disable-autodetect --disable-gpl --disable-nonfree \
    --disable-static --enable-shared --disable-programs --disable-doc \
    --enable-zlib --enable-bzlib "${platform_options[@]}"
make -j"${CARGO_BUILD_JOBS:-$jobs}"
make install
mkdir -p "$prefix/share/lumina-ffmpeg"
cp COPYING.LGPLv2.1 "$prefix/share/lumina-ffmpeg/"
cp "$repo_root/vendor/ffmpeg.lock.json" "$prefix/share/lumina-ffmpeg/"
printf '%s\n' "./configure --disable-autodetect --disable-gpl --disable-nonfree --disable-static --enable-shared --disable-programs --disable-doc --enable-zlib --enable-bzlib ${platform_options[*]}" > "$prefix/share/lumina-ffmpeg/configure.txt"

PKG_CONFIG_PATH="$prefix/lib/pkgconfig" pkg-config --atleast-version=63 libavcodec
