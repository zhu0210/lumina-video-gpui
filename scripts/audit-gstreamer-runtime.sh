#!/usr/bin/env bash
set -euo pipefail

# Static post-build audit used by the remote workflow. The build script performs
# the same checks before packing; this keeps the uploaded artifact independently
# reviewable without unpacking it into the workspace.
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
artifact=${1:-}
source_artifact=${2:-}
lock_file=${3:-$repo_root/vendor/gstreamer-1.0.lock.json}

[[ -n "$artifact" && -n "$source_artifact" ]] || {
    echo "Usage: $0 RUNTIME.tar.xz SOURCES.tar.xz [LOCK]" >&2
    exit 2
}
for command_name in jq tar sha256sum mktemp realpath grep awk; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done
artifact=$(realpath "$artifact")
source_artifact=$(realpath "$source_artifact")
lock_file=$(realpath "$lock_file")
[[ -f "$artifact" && -f "$source_artifact" && -f "$lock_file" ]] || exit 1

schema=$(jq -er '.schema_version' "$lock_file")
[[ "$schema" == 2 ]] || { echo "unsupported audit lock schema" >&2; exit 1; }
for member_archive in "$artifact" "$source_artifact"; do
    members=$(tar -tJf "$member_archive")
    while IFS= read -r member; do
        member=${member#./}
        case "$member" in
            /*|.|..|*/.|*/./*|*/..|*/../*|../*|*//* )
                echo "unsafe archive member: $member" >&2
                exit 1
                ;;
        esac
    done <<<"$members"
done
if tar -tJf "$artifact" | grep -Eiq 'x264|gst-plugins-ugly|nonfree'; then
    echo "forbidden licensing/component name in runtime archive" >&2
    exit 1
fi

manifest=$(tar -xJOf "$artifact" ./runtime-manifest.json)
jq -e \
    '.schema_version == 2 and .policy.gpl == false and .policy.nonfree == false and .policy.unknown == false and .policy.ugly == false and (.variants == ["norust", "alsa", "pulse", "va"]) and (.recursive_dt_needed | type == "array") and (.bundled_files | type == "array")' \
    <<<"$manifest" >/dev/null

runtime_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-runtime-audit.XXXXXX")
source_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-source-audit.XXXXXX")
cleanup() { rm -rf "$runtime_dir" "$source_dir"; }
trap cleanup EXIT
tar -xJf "$artifact" -C "$runtime_dir" --no-same-owner
tar -xJf "$source_artifact" -C "$source_dir" --no-same-owner

while IFS=$'\t' read -r path expected; do
    [[ -f "$runtime_dir/$path" ]] || {
        echo "manifest file is missing: $path" >&2
        exit 1
    }
    actual=$(sha256sum "$runtime_dir/$path" | awk '{print $1}')
    [[ "$actual" == "$expected" ]] || {
        echo "manifest hash mismatch: $path" >&2
        exit 1
    }
done < <(jq -r '.bundled_files[] | [.path, .sha256] | @tsv' <<<"$manifest")

source_manifest=$(jq -e . "$source_dir/source-manifest.json")
[[ "$(jq -r '.runtime_tree_sha256' <<<"$source_manifest")" == "$(jq -r '.tree_sha256' <<<"$manifest")" ]] || {
    echo "source bundle does not correspond to runtime tree" >&2
    exit 1
}
runtime_plugin_dir="$runtime_dir/vendor/linux-x86_64/$(jq -er '.artifact.archive_layout.runtime_libdir' "$lock_file")/gstreamer-1.0"
while IFS= read -r plugin_file; do
    [[ -f "$runtime_plugin_dir/$plugin_file" ]] || {
        echo "audited plugin file is missing: $plugin_file" >&2
        exit 1
    }
done < <(jq -er '.audit.plugin_allowlist[].filename' "$lock_file" | sort -u)
[[ -d "$source_dir/archives" && -d "$source_dir/overlay" ]] || {
    echo "source bundle lacks fetched archives or repository overlay" >&2
    exit 1
}
for license_file in \
    licenses/gstreamer-COPYING \
    licenses/gst-libav-COPYING.LGPL \
    licenses/FFmpeg-COPYING.LGPLv2.1 \
    licenses/zlib-README \
    licenses/PipeWire-LICENSE \
    licenses/overlay/LICENSE.md; do
    [[ -s "$runtime_dir/$license_file" ]] || {
        echo "runtime license text is missing: $license_file" >&2
        exit 1
    }
done

echo "audited runtime and corresponding source archive passed"
