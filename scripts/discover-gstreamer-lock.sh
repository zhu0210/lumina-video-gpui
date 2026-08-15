#!/usr/bin/env bash
set -euo pipefail

# Discovery accepts only a caller-supplied, pinned Cerbero tree/archive. It
# may query small official checksum/tag endpoints, but never downloads a
# Cerbero or component archive. The formal build remains lock-only.
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
lock_file="$repo_root/vendor/gstreamer-1.0.lock.json"
cerbero_dir=
cerbero_archive=
pipewire_archive=

usage() {
    echo "Usage: $0 --cerbero-dir DIR --cerbero-archive ARCHIVE --pipewire-archive ARCHIVE [--lock PATH]" >&2
    exit 2
}
while (($#)); do
    case "$1" in
        --lock) (($# >= 2)) || usage; lock_file=$2; shift 2 ;;
        --cerbero-dir) (($# >= 2)) || usage; cerbero_dir=$2; shift 2 ;;
        --cerbero-archive) (($# >= 2)) || usage; cerbero_archive=$2; shift 2 ;;
        --pipewire-archive) (($# >= 2)) || usage; pipewire_archive=$2; shift 2 ;;
        *) usage ;;
    esac
done
for command_name in curl git jq sha256sum mktemp mv cp rm grep awk sed realpath patch python3; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done
[[ -n "$cerbero_dir" && -n "$cerbero_archive" && -n "$pipewire_archive" ]] || usage

lock_file=$(realpath "$lock_file")
cerbero_dir=$(realpath "$cerbero_dir")
cerbero_archive=$(realpath "$cerbero_archive")
pipewire_archive=$(realpath "$pipewire_archive")
[[ -d "$cerbero_dir/recipes" ]] || { echo "Cerbero tree lacks recipes/: $cerbero_dir" >&2; exit 1; }
[[ -f "$cerbero_archive" && -f "$pipewire_archive" ]] || {
    echo "discovery requires local pinned Cerbero and PipeWire archives" >&2
    exit 1
}

fail() {
    echo "discover-gstreamer-lock: $*" >&2
    exit 1
}

gstreamer_version=$(jq -er '.gstreamer.version' "$lock_file")
[[ "$gstreamer_version" == 1.28.6 ]] || fail "reviewed lock template only supports GStreamer 1.28.6"
gstreamer_url="https://gstreamer.freedesktop.org/src/gstreamer/gstreamer-${gstreamer_version}.tar.xz"
libav_url="https://gstreamer.freedesktop.org/src/gst-libav/gst-libav-${gstreamer_version}.tar.xz"
gstreamer_sha=$(curl -fsSL --retry 3 --max-filesize 1048576 "${gstreamer_url}.sha256sum" | awk 'NR == 1 { print $1 }')
libav_sha=$(curl -fsSL --retry 3 --max-filesize 1048576 "${libav_url}.sha256sum" | awk 'NR == 1 { print $1 }')
[[ "$gstreamer_sha" =~ ^[[:xdigit:]]{64}$ && "$libav_sha" =~ ^[[:xdigit:]]{64}$ ]] || {
    fail "official GStreamer checksums are not SHA-256 digests"
}

cerbero_repo=https://gitlab.freedesktop.org/gstreamer/cerbero.git
tag_json=$(curl -fsSL --retry 3 --max-filesize 1048576 \
    "https://gitlab.freedesktop.org/api/v4/projects/gstreamer%2Fcerbero/repository/tags/${gstreamer_version}")
cerbero_commit=$(jq -er '.commit.id' <<<"$tag_json")
tag_refs=$(git ls-remote --tags "$cerbero_repo" "refs/tags/${gstreamer_version}*")
cerbero_tag_object=$(awk -v ref="refs/tags/${gstreamer_version}" '$2 == ref { print $1 }' <<<"$tag_refs")
cerbero_peeled_commit=$(awk -v ref="refs/tags/${gstreamer_version}^{}" '$2 == ref { print $1 }' <<<"$tag_refs")
[[ "$cerbero_tag_object" =~ ^[[:xdigit:]]{40}$ && "$cerbero_peeled_commit" == "$cerbero_commit" ]] || {
    fail "official Cerbero tag metadata disagrees"
}

pipewire_url="https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/b741e0c74f5436f0c925f7741140db0efd32cf4e/pipewire-b741e0c74f5436f0c925f7741140db0efd32cf4e.tar.gz"
pipewire_tag_commit=$(git ls-remote --tags https://gitlab.freedesktop.org/pipewire/pipewire.git \
    'refs/tags/1.6.8^{}' | awk 'NR == 1 { print $1 }')
[[ "$pipewire_tag_commit" == b741e0c74f5436f0c925f7741140db0efd32cf4e ]] || {
    fail "official PipeWire 1.6.8 tag changed"
}
cerbero_archive_url="https://gitlab.freedesktop.org/gstreamer/cerbero/-/archive/${gstreamer_version}/cerbero-${gstreamer_version}.tar.gz"
cerbero_archive_sha=$(sha256sum "$cerbero_archive" | awk '{ print $1 }')
pipewire_sha=$(sha256sum "$pipewire_archive" | awk '{ print $1 }')

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-lock-discovery.XXXXXX")
cleanup() { rm -rf "$tmp_dir"; }
trap cleanup EXIT
cp -a -- "$cerbero_dir/recipes" "$tmp_dir/recipes"
overlay_dir="$repo_root/vendor/cerbero-overlay"
while IFS= read -r overlay_recipe; do
    [[ -n "$overlay_recipe" ]] || continue
    cp -a -- "$overlay_dir/recipes/$overlay_recipe.recipe" "$tmp_dir/recipes/$overlay_recipe.recipe"
done < <(jq -er '.audit.recipe_metadata[] | select(.overlay == true) | .recipe' "$lock_file")
while IFS= read -r patch_name; do
    [[ -n "$patch_name" ]] || continue
    patch --directory "$tmp_dir" --batch --forward --fuzz=0 --strip=1 \
        <"$overlay_dir/patches/$patch_name" >/dev/null || fail "overlay patch did not apply: $patch_name"
done < <(jq -er '.audit.recipe_metadata[].overlay_patches[]?' "$lock_file")

recipe_facts() {
    python3 - "$1" <<'PY'
import ast
import json
import sys

tree = ast.parse(open(sys.argv[1], encoding="utf-8").read())

def strings(node):
    if isinstance(node, (ast.List, ast.Tuple, ast.Set)):
        return [item.value for item in node.elts if isinstance(item, ast.Constant) and isinstance(item.value, str)]
    return []

facts = {"version": None, "url": None, "sha256": None, "deps": [], "platform_deps": []}
for node in ast.walk(tree):
    if not isinstance(node, ast.Assign):
        continue
    for target in node.targets:
        if not isinstance(target, ast.Name):
            continue
        if target.id == "version" and isinstance(node.value, ast.Constant):
            facts["version"] = node.value.value
        elif target.id == "url" and isinstance(node.value, ast.Constant):
            facts["url"] = node.value.value
        elif target.id == "tarball_checksum" and isinstance(node.value, ast.Constant):
            facts["sha256"] = node.value.value
        elif target.id == "deps":
            facts["deps"] = strings(node.value)
        elif target.id == "platform_deps" and isinstance(node.value, ast.Dict):
            for key, value in zip(node.value.keys, node.value.values):
                if isinstance(key, ast.Attribute) and key.attr == "LINUX":
                    facts["platform_deps"] = strings(value)
print(json.dumps(facts, sort_keys=True))
PY
}

# Parse the pinned archive's actual post-overlay recipe files. This is the
# discovery-side guard against copying guessed URL/version/checksum/dependency
# values into the lock.
while IFS=$'\t' read -r recipe expected_version expected_url expected_sha expected_deps expected_platform; do
    recipe_file="$tmp_dir/recipes/$recipe.recipe"
    [[ -f "$recipe_file" ]] || fail "pinned Cerbero recipe is missing: $recipe"
    facts=$(recipe_facts "$recipe_file")
    [[ "$(jq -r '.sha256 // empty' <<<"$facts")" == "$expected_sha" ]] || fail "recipe checksum disagrees: $recipe"
    actual_version=$(jq -r '.version // empty' <<<"$facts")
    if [[ -n "$actual_version" && "$actual_version" != "$expected_version" ]]; then
        fail "recipe version disagrees: $recipe"
    fi
    [[ "$(jq -c '.deps' <<<"$facts")" == "$expected_deps" ]] || fail "recipe deps disagree: $recipe"
    [[ "$(jq -c '.platform_deps' <<<"$facts")" == "$expected_platform" ]] || fail "recipe Linux platform deps disagree: $recipe"
    raw_url=$(jq -r '.url // empty' <<<"$facts")
    case "$raw_url" in
        ''|gnome://*|xiph://*|*'%(*)'*) ;;
        *) [[ "$raw_url" == "$expected_url" ]] || fail "recipe URL disagrees: $recipe" ;;
    esac
    jq -e --arg recipe "$recipe" --arg url "$expected_url" --arg sha "$expected_sha" \
        'any(.components[]; .recipe == $recipe and .source_url == $url and .sha256 == $sha)' \
        "$lock_file" >/dev/null || fail "component source metadata disagrees: $recipe"
done < <(jq -er '.audit.recipe_metadata[] | [.recipe, .version, .source_url, .sha256, (.deps | tojson), (.platform_deps | tojson)] | @tsv' "$lock_file")

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
        if .recipe == "gstreamer-1.0" then .version = $version | .source_url = $gstreamer_url | .sha256 = $gstreamer_sha
        elif .recipe == "gst-libav-1.0" then .version = $version | .source_url = $libav_url | .sha256 = $libav_sha
        elif .recipe == "pipewire" then .tag_commit = $pipewire_commit | .source_url = $pipewire_url | .sha256 = $pipewire_sha
        else . end)
    | .audit.recipe_metadata |= map(
        if .recipe == "gstreamer-1.0" then .version = $version | .source_url = $gstreamer_url | .sha256 = $gstreamer_sha
        elif .recipe == "gst-libav-1.0" then .version = $version | .source_url = $libav_url | .sha256 = $libav_sha
        elif .recipe == "pipewire" then .source_url = $pipewire_url | .sha256 = $pipewire_sha
        else . end)' "$lock_file" >"$lock_tmp"
mv -f "$lock_tmp" "$lock_file"
trap - EXIT
echo "verified all locked Cerbero recipes and updated official metadata in $lock_file"
