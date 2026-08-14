#!/usr/bin/env bash
set -euo pipefail

# Discovery is the only moving-metadata path. It checks official release/tag
# metadata, then updates the already-reviewed schema-2 lock template. The
# formal build never invokes this script.
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
lock_file="$repo_root/vendor/gstreamer-1.0.lock.json"

usage() {
    echo "Usage: $0 [--lock PATH]" >&2
    exit 2
}
while (($#)); do
    case "$1" in
        --lock) (($# >= 2)) || usage; lock_file=$2; shift 2 ;;
        *) usage ;;
    esac
done
for command_name in curl git jq sha256sum mktemp mv grep awk sed realpath; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done

lock_file=$(realpath "$lock_file")
gstreamer_version=$(jq -er '.gstreamer.version' "$lock_file")
[[ "$gstreamer_version" == 1.28.6 ]] || {
    echo "reviewed lock template only supports GStreamer 1.28.6" >&2
    exit 1
}
gstreamer_url="https://gstreamer.freedesktop.org/src/gstreamer/gstreamer-${gstreamer_version}.tar.xz"
libav_url="https://gstreamer.freedesktop.org/src/gst-libav/gst-libav-${gstreamer_version}.tar.xz"
gstreamer_sha=$(curl -fsSL --retry 3 "${gstreamer_url}.sha256sum" | awk 'NR == 1 { print $1 }')
libav_sha=$(curl -fsSL --retry 3 "${libav_url}.sha256sum" | awk 'NR == 1 { print $1 }')
[[ "$gstreamer_sha" =~ ^[[:xdigit:]]{64}$ && "$libav_sha" =~ ^[[:xdigit:]]{64}$ ]] || {
    echo "official GStreamer checksums are not SHA-256 digests" >&2
    exit 1
}

cerbero_repo=https://gitlab.freedesktop.org/gstreamer/cerbero.git
tag_json=$(curl -fsSL --retry 3 \
    "https://gitlab.freedesktop.org/api/v4/projects/gstreamer%2Fcerbero/repository/tags/${gstreamer_version}")
cerbero_commit=$(jq -er '.commit.id' <<<"$tag_json")
tag_refs=$(git ls-remote --tags "$cerbero_repo" "refs/tags/${gstreamer_version}*")
cerbero_tag_object=$(awk -v ref="refs/tags/${gstreamer_version}" '$2 == ref { print $1 }' <<<"$tag_refs")
cerbero_peeled_commit=$(awk -v ref="refs/tags/${gstreamer_version}^{}" '$2 == ref { print $1 }' <<<"$tag_refs")
[[ "$cerbero_tag_object" =~ ^[[:xdigit:]]{40}$ && "$cerbero_peeled_commit" == "$cerbero_commit" ]] || {
    echo "official Cerbero tag metadata disagrees" >&2
    exit 1
}

pipewire_url="https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/1.6.8/pipewire-1.6.8.tar.gz"
pipewire_tag_commit=$(git ls-remote --tags https://gitlab.freedesktop.org/pipewire/pipewire.git \
    'refs/tags/1.6.8^{}' | awk 'NR == 1 { print $1 }')
[[ "$pipewire_tag_commit" == b741e0c74f5436f0c925f7741140db0efd32cf4e ]] || {
    echo "official PipeWire 1.6.8 tag changed" >&2
    exit 1
}

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-lock-discovery.XXXXXX")
cleanup() { rm -rf "$tmp_dir"; }
trap cleanup EXIT
cerbero_archive_url="https://gitlab.freedesktop.org/gstreamer/cerbero/-/archive/${gstreamer_version}/cerbero-${gstreamer_version}.tar.gz"
curl -fL --retry 3 --max-filesize 50000000 "$cerbero_archive_url" -o "$tmp_dir/cerbero.tar.gz"
cerbero_archive_sha=$(sha256sum "$tmp_dir/cerbero.tar.gz" | awk '{ print $1 }')
curl -fL --retry 3 --max-filesize 10000000 "$pipewire_url" -o "$tmp_dir/pipewire.tar.gz"
pipewire_sha=$(sha256sum "$tmp_dir/pipewire.tar.gz" | awk '{ print $1 }')
[[ "$cerbero_archive_sha" =~ ^[[:xdigit:]]{64}$ && "$pipewire_sha" =~ ^[[:xdigit:]]{64}$ ]] || {
    echo "official archive checksum is not SHA-256" >&2
    exit 1
}

lock_tmp=$(mktemp "${lock_file}.XXXXXX")
cleanup_lock() { rm -f "$lock_tmp"; }
trap cleanup_lock EXIT
jq \
    --arg version "$gstreamer_version" \
    --arg gstreamer_url "$gstreamer_url" \
    --arg gstreamer_sha "$gstreamer_sha" \
    --arg libav_url "$libav_url" \
    --arg libav_sha "$libav_sha" \
    --arg cerbero_repo "$cerbero_repo" \
    --arg cerbero_tag_object "$cerbero_tag_object" \
    --arg cerbero_commit "$cerbero_commit" \
    --arg cerbero_archive_url "$cerbero_archive_url" \
    --arg cerbero_archive_sha "$cerbero_archive_sha" \
    --arg pipewire_url "$pipewire_url" \
    --arg pipewire_sha "$pipewire_sha" \
    --arg pipewire_commit "$pipewire_tag_commit" \
    ' .gstreamer.version = $version
    | .gstreamer.source.url = $gstreamer_url
    | .gstreamer.source.sha256 = $gstreamer_sha
    | .sources.gst_libav.url = $libav_url
    | .sources.gst_libav.sha256 = $libav_sha
    | .sources.pipewire.url = $pipewire_url
    | .sources.pipewire.sha256 = $pipewire_sha
    | .sources.pipewire.tag_commit = $pipewire_commit
    | .cerbero.repository = $cerbero_repo
    | .cerbero.tag_object = $cerbero_tag_object
    | .cerbero.commit = $cerbero_commit
    | .cerbero.archive.url = $cerbero_archive_url
    | .cerbero.archive.sha256 = $cerbero_archive_sha
    | .components |= map(
        if .name == "gstreamer" then .version = $version | .source_url = $gstreamer_url | .sha256 = $gstreamer_sha | .cache_path = ("gstreamer-1.0/gstreamer-" + $version + ".tar.xz")
        elif .name == "gst-libav" then .version = $version | .source_url = $libav_url | .sha256 = $libav_sha | .cache_path = ("gst-libav-1.0/gst-libav-" + $version + ".tar.xz")
        elif .name == "PipeWire" then .tag_commit = $pipewire_commit | .source_url = $pipewire_url | .sha256 = $pipewire_sha
        else . end)' "$lock_file" >"$lock_tmp"
mv -f "$lock_tmp" "$lock_file"
trap - EXIT
echo "updated official GStreamer/Cerbero/PipeWire metadata in $lock_file"
