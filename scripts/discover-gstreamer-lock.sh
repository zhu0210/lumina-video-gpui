#!/usr/bin/env bash
set -euo pipefail

# Discovery is deliberately separate from the formal build. It may inspect
# upstream moving metadata, then atomically replaces the reproducibility lock.

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
lock_file="$repo_root/vendor/gstreamer-1.0.lock.json"

usage() {
    echo "Usage: $0 [--lock PATH]" >&2
    exit 2
}

while (($#)); do
    case "$1" in
        --lock)
            (($# >= 2)) || usage
            lock_file=$2
            shift 2
            ;;
        *) usage ;;
    esac
done

for command_name in curl git jq sha256sum sort mktemp mv grep awk sed tar; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done

download_page=$(curl -fsSL --retry 3 https://gstreamer.freedesktop.org/download/)
source_index=$(curl -fsSL --retry 3 https://gstreamer.freedesktop.org/src/gstreamer/)
versions=$(
    {
        printf '%s\n' "$download_page" "$source_index" |
            grep -oE '1\.28\.[0-9]+' || true
    } | sort -Vu
)
gstreamer_version=$(printf '%s\n' "$versions" | awk 'END { print }')
[[ "$gstreamer_version" =~ ^1\.28\.[0-9]+$ ]] || {
    echo "could not discover an official GStreamer 1.28.x version" >&2
    exit 1
}

gstreamer_url="https://gstreamer.freedesktop.org/src/gstreamer/gstreamer-${gstreamer_version}.tar.xz"
gstreamer_sha=$(curl -fsSL --retry 3 "${gstreamer_url}.sha256sum" | awk 'NR == 1 { print $1 }')
libav_url="https://gstreamer.freedesktop.org/src/gst-libav/gst-libav-${gstreamer_version}.tar.xz"
libav_sha=$(curl -fsSL --retry 3 "${libav_url}.sha256sum" | awk 'NR == 1 { print $1 }')
[[ "$gstreamer_sha" =~ ^[[:xdigit:]]{64}$ ]] || {
    echo "official GStreamer checksum is not a SHA-256 digest" >&2
    exit 1
}
[[ "$libav_sha" =~ ^[[:xdigit:]]{64}$ ]] || {
    echo "official gst-libav checksum is not a SHA-256 digest" >&2
    exit 1
}

cerbero_repo=https://gitlab.freedesktop.org/gstreamer/cerbero.git
tag_json=$(curl -fsSL --retry 3 \
    "https://gitlab.freedesktop.org/api/v4/projects/gstreamer%2Fcerbero/repository/tags/${gstreamer_version}")
cerbero_commit=$(jq -er '.commit.id' <<<"$tag_json")
tag_refs=$(git ls-remote --tags "$cerbero_repo" "refs/tags/${gstreamer_version}*")
cerbero_tag_object=$(awk -v ref="refs/tags/${gstreamer_version}" '$2 == ref { print $1 }' <<<"$tag_refs")
cerbero_peeled_commit=$(awk -v ref="refs/tags/${gstreamer_version}^{}" '$2 == ref { print $1 }' <<<"$tag_refs")
[[ "$cerbero_tag_object" =~ ^[[:xdigit:]]{40}$ ]] || {
    echo "annotated Cerbero tag object was not found" >&2
    exit 1
}
[[ "$cerbero_peeled_commit" =~ ^[[:xdigit:]]{40}$ ]] || {
    echo "peeled Cerbero tag commit was not found" >&2
    exit 1
}
[[ "$cerbero_commit" == "$cerbero_peeled_commit" ]] || {
    echo "GitLab tag API and peeled commit disagree" >&2
    exit 1
}

cerbero_archive_url="https://gitlab.freedesktop.org/gstreamer/cerbero/-/archive/${gstreamer_version}/cerbero-${gstreamer_version}.tar.gz"
archive_tmp=$(mktemp)
mkdir -p "$(dirname -- "$lock_file")"
lock_tmp=$(mktemp "${lock_file}.XXXXXX")
cleanup() {
    rm -f "$archive_tmp" "$lock_tmp"
}
trap cleanup EXIT
curl -fL --retry 3 --max-filesize 50000000 "$cerbero_archive_url" -o "$archive_tmp"
cerbero_archive_sha=$(sha256sum "$archive_tmp" | awk '{ print $1 }')
zlib_recipe=$(tar -xOf "$archive_tmp" --wildcards '*/recipes/zlib.recipe')
zlib_version=$(sed -n "s/^[[:space:]]*version = '\([^']*\)'$/\1/p" <<<"$zlib_recipe")
zlib_sha=$(sed -n "s/^[[:space:]]*tarball_checksum = '\([^']*\)'$/\1/p" <<<"$zlib_recipe")
[[ "$zlib_version" == 1.3.1 ]] || {
    echo "Cerbero zlib recipe is not the approved 1.3.1 route" >&2
    exit 1
}
[[ "$zlib_sha" == 9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23 ]] || {
    echo "Cerbero zlib recipe checksum changed" >&2
    exit 1
}
zlib_filename="zlib-${zlib_version}.tar.gz"
zlib_url="https://gstreamer.freedesktop.org/src/mirror/zlib/${zlib_filename}"

registry=https://registry-1.docker.io
token=$(curl -fsSL --retry 3 \
    'https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/ubuntu:pull' |
    jq -er '.token')
manifest=$(curl -fsSL --retry 3 \
    -H "Authorization: Bearer $token" \
    -H 'Accept: application/vnd.docker.distribution.manifest.list.v2+json' \
    "$registry/v2/library/ubuntu/manifests/24.04")
ubuntu_digest=$(jq -er \
    '[.manifests[] | select(.platform.os == "linux" and .platform.architecture == "amd64")] | first | .digest' \
    <<<"$manifest")
[[ "$ubuntu_digest" =~ ^sha256:[[:xdigit:]]{64}$ ]] || {
    echo "Ubuntu 24.04 amd64 manifest has no immutable digest" >&2
    exit 1
}

required_elements='[
  {"name":"playbin3","filename":"libgstplayback.so","purpose":"playback graph"},
  {"name":"qtdemux","filename":"libgstisomp4.so","purpose":"MP4 demux"},
  {"name":"h264parse","filename":"libgstvideoparsersbad.so","purpose":"H.264 parser"},
  {"name":"avdec_h264","filename":"libgstlibav.so","purpose":"H.264 decoder"},
  {"name":"aacparse","filename":"libgstaudioparsers.so","purpose":"AAC parser"},
  {"name":"avdec_aac","filename":"libgstlibav.so","purpose":"AAC decoder"},
  {"name":"fakesink","filename":"libgstcoreelements.so","purpose":"headless smoke sink"}
]'

jq -n \
    --arg version "$gstreamer_version" \
    --arg gstreamer_url "$gstreamer_url" \
    --arg gstreamer_sha "$gstreamer_sha" \
    --arg libav_url "$libav_url" \
    --arg libav_sha "$libav_sha" \
    --arg zlib_version "$zlib_version" \
    --arg zlib_filename "$zlib_filename" \
    --arg zlib_url "$zlib_url" \
    --arg zlib_sha "$zlib_sha" \
    --arg cerbero_repo "$cerbero_repo" \
    --arg cerbero_tag "$gstreamer_version" \
    --arg cerbero_tag_object "$cerbero_tag_object" \
    --arg cerbero_commit "$cerbero_commit" \
    --arg cerbero_archive_url "$cerbero_archive_url" \
    --arg cerbero_archive_sha "$cerbero_archive_sha" \
    --arg ubuntu_digest "$ubuntu_digest" \
    --argjson required_elements "$required_elements" \
    '{
      schema_version: 1,
      gstreamer: {version: $version, source: {url: $gstreamer_url, sha256: $gstreamer_sha}},
      sources: {
        gst_libav: {package: "gst-libav-1.0", filename: ("gst-libav-" + $version + ".tar.xz"), url: $libav_url, sha256: $libav_sha},
        zlib: {version: $zlib_version, filename: $zlib_filename, url: $zlib_url, sha256: $zlib_sha}
      },
      cerbero: {repository: $cerbero_repo, tag: $cerbero_tag, tag_object: $cerbero_tag_object, commit: $cerbero_commit, archive: {url: $cerbero_archive_url, sha256: $cerbero_archive_sha}},
      target: {os: "linux", architecture: "x86_64", distribution: "ubuntu", distribution_version: "24.04", glibc: "2.39"},
      builder: {image: ("ubuntu@" + $ubuntu_digest), platform: "linux/amd64"},
      packages: ["gstreamer-1.0", "gstreamer-1.0-libav"],
      artifact: {type: "tarball", compression: "xz", split: false},
      variants: ["norust"],
      required_elements: $required_elements
    }' >"$lock_tmp"
mv -f "$lock_tmp" "$lock_file"
trap - EXIT
rm -f "$archive_tmp"
echo "wrote $lock_file for GStreamer $gstreamer_version"
