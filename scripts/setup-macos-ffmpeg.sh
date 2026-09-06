#!/usr/bin/env bash
# Build the same locked FFmpeg used by CI and the macOS app bundle.
set -euo pipefail
[[ "$(uname -s)" == Darwin ]] || { echo "macOS is required" >&2; exit 1; }
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
brew install nasm pkg-config
prefix="$repo_root/target/ffmpeg-runtime"
bash "$script_dir/build-ffmpeg-runtime.sh" "$prefix"
printf 'Build with: PKG_CONFIG_PATH=%q/lib/pkgconfig LIBRARY_PATH=%q/lib cargo build\n' "$prefix" "$prefix"
