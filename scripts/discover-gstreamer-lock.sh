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
pulseaudio_archive=

usage() {
    echo "Usage: $0 --cerbero-dir DIR --cerbero-archive ARCHIVE --pipewire-archive ARCHIVE --pulseaudio-archive ARCHIVE [--lock PATH]" >&2
    exit 2
}

verify_overlay_copy() {
    local relative_path=$1 materialized_path=$2 expected_sha actual_sha
    expected_sha=$(awk -F '\t' -v path="$relative_path" '$1 == path { print $2 }' "$overlay_input_list")
    [[ -n "$expected_sha" ]] || fail "materialized overlay path is not lock-owned: $relative_path"
    [[ -f "$materialized_path" && ! -L "$materialized_path" ]] || fail "materialized overlay control is not a regular file: $relative_path"
    if ! actual_sha=$(sha256sum -- "$materialized_path" | awk '{ print $1 }'); then
        fail "could not hash materialized overlay control: $relative_path"
    fi
    [[ "$actual_sha" == "$expected_sha" ]] || fail "materialized overlay control hash mismatch: $relative_path"
}
while (($#)); do
    case "$1" in
        --lock) (($# >= 2)) || usage; lock_file=$2; shift 2 ;;
        --cerbero-dir) (($# >= 2)) || usage; cerbero_dir=$2; shift 2 ;;
        --cerbero-archive) (($# >= 2)) || usage; cerbero_archive=$2; shift 2 ;;
        --pipewire-archive) (($# >= 2)) || usage; pipewire_archive=$2; shift 2 ;;
        --pulseaudio-archive) (($# >= 2)) || usage; pulseaudio_archive=$2; shift 2 ;;
        *) usage ;;
    esac
done
for command_name in curl git jq sha256sum tar mktemp mv cp rm grep awk sed realpath patch python3 cmp find sort; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done
[[ -n "$cerbero_dir" && -n "$cerbero_archive" && -n "$pipewire_archive" && -n "$pulseaudio_archive" ]] || usage
overlay_dir="$repo_root/vendor/cerbero-overlay"

lock_file=$(realpath "$lock_file")
cerbero_dir=$(realpath "$cerbero_dir")
cerbero_archive=$(realpath "$cerbero_archive")
pipewire_archive=$(realpath "$pipewire_archive")
pulseaudio_archive=$(realpath "$pulseaudio_archive")
[[ -d "$cerbero_dir/recipes" ]] || { echo "Cerbero tree lacks recipes/: $cerbero_dir" >&2; exit 1; }
[[ -f "$cerbero_archive" && -f "$pipewire_archive" && -f "$pulseaudio_archive" ]] || {
    echo "discovery requires local pinned Cerbero, PipeWire, and PulseAudio archives" >&2
    exit 1
}

fail() {
    echo "discover-gstreamer-lock: $*" >&2
    exit 1
}
validate_overlay_inputs() {
    local overlay_root=$1 lock_path=$2 destination=$3 source_root=$4 snapshot_root=$5
    local actual_file_list="${destination}.filesystem"
    local normalized_file_list="${destination}.normalized"
    local listed_file_list="${destination}.listed"
    local snapshot_file_list="${destination}.snapshot"
    local snapshot_normalized_file_list="${destination}.snapshot.normalized"
    local path expected_sha actual_sha snapshot_sha absolute source_path snapshot_path metadata_name

    if ! jq -er '
        .audit.overlay_inputs as $items
        | if ($items | type) != "array" or ($items | length) != 16 then
            error("overlay input manifest must contain exactly 16 files")
          elif any($items[]; (.path | type) != "string" or (.sha256 | type) != "string") then
            error("overlay input manifest has invalid fields")
          elif any($items[]; (.path | test("^vendor/cerbero-overlay/(config|packages|recipes|patches)/[^/]+$") | not)) then
            error("overlay input path is outside the control directories")
          elif any($items[]; (.path | test("(^|/)\\.\\.?(/|$)"))) then
            error("overlay input path contains traversal")
          elif any($items[]; (.sha256 | test("^[0-9a-f]{64}$") | not)) then
            error("overlay input hash is not lowercase SHA-256")
          elif (($items | map(.path) | length) != ($items | map(.path) | unique | length)) then
            error("overlay input paths are not unique")
          else $items[] | [.path, .sha256] | @tsv
          end
    ' "$lock_path" >"$destination"; then
        fail "invalid audited overlay input hash manifest"
    fi

    [[ ! -e "$snapshot_root" ]] || fail "overlay snapshot path already exists"
    if ! mkdir -p "$snapshot_root"; then
        fail "could not create private overlay snapshot"
    fi
    : >"$actual_file_list"
    for directory in config packages recipes patches; do
        if ! find "$overlay_root/$directory" -type f -print >>"$actual_file_list"; then
            fail "could not enumerate audited overlay controls"
        fi
    done
    while IFS= read -r absolute; do
        case "$absolute" in
            "$source_root"/*)
                printf '%s\n' "${absolute#"$source_root"/}"
                ;;
            *)
                fail "overlay control is outside the repository"
                ;;
        esac
    done <"$actual_file_list" >"$normalized_file_list"
    sort -o "$normalized_file_list" "$normalized_file_list"
    if ! awk -F '\t' '{ print $1 }' "$destination" | sort >"$listed_file_list"; then
        fail "could not normalize audited overlay input paths"
    fi
    cmp -s "$normalized_file_list" "$listed_file_list" || {
        fail "audited overlay file set differs from the lock"
    }
    if ! awk 'END { exit !(NR == 16) }' "$destination"; then
        fail "audited overlay input manifest has an unexpected size"
    fi
    while IFS=$'\t' read -r path expected_sha; do
        source_path="$source_root/$path"
        snapshot_path="$snapshot_root/$path"
        [[ -f "$source_path" && ! -L "$source_path" ]] || fail "missing audited overlay control: $path"
        if ! actual_sha=$(sha256sum -- "$source_path" | awk '{ print $1 }'); then
            fail "could not hash audited overlay control: $path"
        fi
        [[ "$actual_sha" == "$expected_sha" ]] || fail "audited overlay control hash mismatch: $path"
        mkdir -p "${snapshot_path%/*}"
        if ! cp -- "$source_path" "$snapshot_path"; then
            fail "could not snapshot audited overlay control: $path"
        fi
        [[ -f "$snapshot_path" && ! -L "$snapshot_path" ]] || fail "snapshot control is not a regular file: $path"
        if ! snapshot_sha=$(sha256sum -- "$snapshot_path" | awk '{ print $1 }'); then
            fail "could not hash snapshot overlay control: $path"
        fi
        [[ "$snapshot_sha" == "$expected_sha" ]] || fail "snapshot overlay control hash mismatch: $path"
    done <"$destination"
    for metadata_name in README.md LICENSE.md; do
        source_path="$overlay_root/$metadata_name"
        snapshot_path="$snapshot_root/vendor/cerbero-overlay/$metadata_name"
        [[ -f "$source_path" && ! -L "$source_path" ]] || fail "overlay metadata is missing: $metadata_name"
        if ! cp -- "$source_path" "$snapshot_path"; then
            fail "could not snapshot overlay metadata: $metadata_name"
        fi
        [[ -f "$snapshot_path" && ! -L "$snapshot_path" ]] || fail "snapshot metadata is not a regular file: $metadata_name"
    done
    : >"$snapshot_file_list"
    for directory in config packages recipes patches; do
        if ! find "$snapshot_root/vendor/cerbero-overlay/$directory" -type f -print >>"$snapshot_file_list"; then
            fail "could not enumerate overlay snapshot controls"
        fi
    done
    while IFS= read -r absolute; do
        case "$absolute" in
            "$snapshot_root"/*)
                printf '%s\n' "${absolute#"$snapshot_root"/}"
                ;;
            *)
                fail "overlay snapshot escapes its private root"
                ;;
        esac
    done <"$snapshot_file_list" >"$snapshot_normalized_file_list"
    sort -o "$snapshot_normalized_file_list" "$snapshot_normalized_file_list"
    cmp -s "$snapshot_normalized_file_list" "$listed_file_list" || {
        fail "overlay snapshot file set differs from the lock"
    }
}
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-lock-discovery.XXXXXX")
cleanup() { rm -rf "$tmp_dir"; }
trap cleanup EXIT
overlay_input_list="$tmp_dir/overlay-inputs.tsv"
overlay_snapshot="$tmp_dir/overlay-snapshot"
validate_overlay_inputs "$overlay_dir" "$lock_file" "$overlay_input_list" "$repo_root" "$overlay_snapshot"
overlay_dir="$overlay_snapshot/vendor/cerbero-overlay"

# Keep this verifier local so discovery remains independently auditable.
package_files_from_overlay() {
    python3 - "$1" <<'PY'
import ast
import json
import sys

tree = ast.parse(open(sys.argv[1], encoding="utf-8").read())
package_classes = [node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == "Package"]
if len(package_classes) != 1:
    raise SystemExit("audited package class must be unique")
assignments = [
    node for node in package_classes[0].body
    if isinstance(node, ast.Assign)
    and len(node.targets) == 1
    and isinstance(node.targets[0], ast.Name)
    and node.targets[0].id == "files"
]
if len(assignments) != 1:
    raise SystemExit("package files assignment must be unique")
allowed_target = assignments[0].targets[0]
value = assignments[0].value
if not isinstance(value, (ast.List, ast.Tuple)):
    raise SystemExit("package files assignment must be a constant list")
files = []
for item in value.elts:
    if not isinstance(item, ast.Constant) or not isinstance(item.value, str):
        raise SystemExit("package files list must contain only strings")
    files.append(item.value)
if len(files) != len(set(files)):
    raise SystemExit("package files list contains duplicates")
for node in ast.walk(package_classes[0]):
    if isinstance(node, ast.Name) and node.id == "files" and node is not allowed_target:
        raise SystemExit("package files name is mutated or read outside its declaration")
json.dump(files, sys.stdout, separators=(",", ":"))
print()
PY
}

gstreamer_version=$(jq -er '.gstreamer.version' "$lock_file")
[[ "$gstreamer_version" == 1.28.6 ]] || fail "reviewed lock template only supports GStreamer 1.28.6"
jq -e '.variants == ["norust", "nogi", "nounwind", "alsa", "pulse", "va"]' "$lock_file" >/dev/null || {
    fail "lock variants are not the exact audited set"
}
jq -e '(.components | length == 29) and all(.components[]; (.recipe != "bash-completion" and .recipe != "libunwind" and .recipe != "gobject-introspection"))' "$lock_file" >/dev/null || {
    fail "lock must contain exactly the 29 audited runtime components"
}
jq -e '
    ([.audit.recipe_metadata[] | select(has("archive_root"))] as $roots
     | ($roots | length == 7)
     and (($roots | map(.recipe) | sort) == ["alsa", "libdrm", "libpulse", "libsndfile", "libva", "openssl", "pipewire"])
     and all($roots[]; (.archive_root | (type == "string" and length > 0)))
     and (($roots | map(.archive_root) | unique | length) == ($roots | length))
     and (([.audit.recipe_metadata[] | select(.overlay == true) | .recipe] | sort) == ["alsa", "libdrm", "libpulse", "libsndfile", "libva", "pipewire"])
    )
' "$lock_file" >/dev/null || fail "lock archive-root metadata is incomplete"
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
pulse_repo=https://gitlab.freedesktop.org/pulseaudio/pulseaudio.git
pulse_tag=v17.0
pulse_tag_refs=$(git ls-remote --tags "$pulse_repo" "refs/tags/$pulse_tag" "refs/tags/$pulse_tag^{}")
pulse_tag_object=$(awk -v ref="refs/tags/$pulse_tag" '$2 == ref { print $1 }' <<<"$pulse_tag_refs")
pulse_tag_commit=$(awk -v ref="refs/tags/$pulse_tag^{}" '$2 == ref { print $1 }' <<<"$pulse_tag_refs")
[[ "$pulse_tag_object" == 16be4f7accce287fd08519591c6356ffa61aaaf1 && \
   "$pulse_tag_commit" == 1f020889c9aa44ea0f63d7222e8c2b62c3f45f68 ]] || {
    fail "official PulseAudio v17.0 tag metadata disagrees"
}
pulse_signature_json=$(curl -fsSL --max-filesize 1048576 \
    "https://gitlab.freedesktop.org/api/v4/projects/pulseaudio%2Fpulseaudio/repository/tags/$pulse_tag/signature")
pulse_signature=$(jq -er 'if type == "object" and .signature_type == "PGP" then .signature_type else error("tag signature is not PGP") end' \
    <<<"$pulse_signature_json") || fail "official PulseAudio annotated tag signature is not PGP"
pulse_url="https://gitlab.freedesktop.org/pulseaudio/pulseaudio/-/archive/$pulse_tag_commit/pulseaudio-$pulse_tag_commit.tar.gz"
pulse_sha=$(sha256sum "$pulseaudio_archive" | awk '{ print $1 }')
pulse_archive_root="pulseaudio-$pulse_tag_commit"
verify_pulse_archive() {
    local members root meson_file license_file raw_meson raw_license
    members="$tmp_dir/pulseaudio-members"
    if ! tar -tzf "$pulseaudio_archive" >"$members"; then
        fail "could not list the caller-supplied PulseAudio archive"
    fi
    root=$(awk -F/ 'NF { print $1; exit }' "$members")
    [[ "$root" == "$pulse_archive_root" ]] || fail "PulseAudio archive root disagrees with the pinned commit archive"
    if ! awk -F/ -v expected="$pulse_archive_root" '$1 != expected { invalid = 1 } END { exit invalid ? 1 : 0 }' "$members"; then
        fail "PulseAudio archive contains an unexpected top-level path"
    fi
    if grep -Fx "$pulse_archive_root/.tarball-version" "$members" >/dev/null; then
        fail "PulseAudio archive unexpectedly contains .tarball-version"
    fi
    [[ "$pulse_sha" == 0ccee8a0c9653badc668cf11f0eaad97a1febb85afa82c24c2d1935926446e3b ]] || {
        fail "caller-supplied PulseAudio archive checksum is not the pinned commit archive"
    }
    meson_file="$tmp_dir/pulseaudio-meson.build"
    license_file="$tmp_dir/pulseaudio-LGPL"
    raw_meson="$tmp_dir/pulseaudio-meson.raw"
    raw_license="$tmp_dir/pulseaudio-LGPL.raw"
    tar -xOf "$pulseaudio_archive" "$pulse_archive_root/meson.build" >"$meson_file" || fail "PulseAudio meson.build is missing"
    tar -xOf "$pulseaudio_archive" "$pulse_archive_root/LGPL" >"$license_file" || fail "PulseAudio LGPL text is missing"
    curl -fsSL --max-filesize 1048576 \
        "https://gitlab.freedesktop.org/pulseaudio/pulseaudio/-/raw/$pulse_tag_commit/meson.build" >"$raw_meson" || fail "PulseAudio commit meson.build fetch failed"
    curl -fsSL --max-filesize 1048576 \
        "https://gitlab.freedesktop.org/pulseaudio/pulseaudio/-/raw/$pulse_tag_commit/LGPL" >"$raw_license" || fail "PulseAudio commit LGPL fetch failed"
    cmp -s "$meson_file" "$raw_meson" || fail "PulseAudio archive meson.build differs from the pinned commit raw file"
    cmp -s "$license_file" "$raw_license" || fail "PulseAudio archive LGPL differs from the pinned commit raw file"
    [[ "$(sha256sum "$meson_file" | awk '{ print $1 }')" == 33318f0c2019939d46ea38acb8a6d1e43198d9d6772a1ef56d1d1618f05a174a ]] || fail "PulseAudio meson.build bytes are not pinned"
    [[ "$(sha256sum "$license_file" | awk '{ print $1 }')" == a9bdde5616ecdd1e980b44f360600ee8783b1f99b8cc83a2beb163a0a390e861 ]] || fail "PulseAudio LGPL bytes are not pinned"
}
verify_pulse_archive
cerbero_archive_url="https://gitlab.freedesktop.org/gstreamer/cerbero/-/archive/${gstreamer_version}/cerbero-${gstreamer_version}.tar.gz"
cerbero_archive_sha=$(sha256sum "$cerbero_archive" | awk '{ print $1 }')
pipewire_sha=$(sha256sum "$pipewire_archive" | awk '{ print $1 }')

cp -a -- "$cerbero_dir/recipes" "$tmp_dir/recipes"
overlay_package="$overlay_dir/packages/lumina-audited.package"
[[ -f "$overlay_package" ]] || fail "audited Cerbero package is missing"
if grep -Eq '^[[:space:]]*deps[[:space:]]*=' "$overlay_package"; then
    fail "audited private package must not depend on an upstream package"
fi
overlay_package_files="$tmp_dir/overlay-package-files.json"
if ! package_files_from_overlay "$overlay_package" >"$overlay_package_files"; then
    fail "audited package files list is not a unique constant string list"
fi
if ! jq -e --slurpfile actual "$overlay_package_files" \
    '$actual[0] == .audit.package_files' "$lock_file" >/dev/null; then
    fail "audited package files do not exactly match the lock"
fi
if ! jq -er 'if type == "array" and all(.[]; type == "string") then .[] else error("invalid package files") end' \
    "$overlay_package_files" >"$tmp_dir/validated-package-file-specs"; then
    fail "validated package files could not be materialized"
fi
while IFS= read -r overlay_recipe; do
    [[ -n "$overlay_recipe" ]] || continue
    cp -a -- "$overlay_dir/recipes/$overlay_recipe.recipe" "$tmp_dir/recipes/$overlay_recipe.recipe"
    verify_overlay_copy "vendor/cerbero-overlay/recipes/$overlay_recipe.recipe" "$tmp_dir/recipes/$overlay_recipe.recipe"
done < <(jq -er '.audit.recipe_metadata[] | select(.overlay == true) | .recipe' "$lock_file")
if ! python3 - "$tmp_dir/recipes/libpulse.recipe" <<'PY'
import ast
import sys

tree = ast.parse(open(sys.argv[1], encoding="utf-8").read())
recipes = [node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == "Recipe"]
if len(recipes) != 1:
    raise SystemExit("PulseAudio recipe class is not unique")
base = recipes[0].bases
if len(base) != 1 or not (
    isinstance(base[0], ast.Attribute)
    and isinstance(base[0].value, ast.Name)
    and base[0].value.id == "recipe"
    and base[0].attr == "Recipe"
):
    raise SystemExit("PulseAudio recipe uses a system or host fallback")
set_env_calls = []
for node in ast.walk(tree):
    if isinstance(node, ast.Name) and node.id in {"SystemRecipe", "system_recipe", "allow_system_recipes"}:
        raise SystemExit("PulseAudio recipe references a system fallback")
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
        if node.func.attr in {"use_system", "system_recipe", "get_system_recipe"}:
            raise SystemExit("PulseAudio recipe calls a system fallback")
        if node.func.attr == "set_env":
            set_env_calls.append(node)
if len(set_env_calls) != 1:
    raise SystemExit("PulseAudio release environment assignment is not unique")
call = set_env_calls[0]
if len(call.args) != 2 or any(not isinstance(arg, ast.Constant) or not isinstance(arg.value, str) for arg in call.args):
    raise SystemExit("PulseAudio release environment assignment is not literal")
if [arg.value for arg in call.args] != ["GIT_DESCRIBE_FOR_BUILD", "v17.0"]:
    raise SystemExit("PulseAudio release environment is not v17.0")
PY
then
    fail "pinned PulseAudio recipe is not the exact signed-tag client recipe"
fi
while IFS= read -r patch_name; do
    [[ -n "$patch_name" ]] || continue
    patch --directory "$tmp_dir" --batch --forward --fuzz=0 --strip=1 \
        <"$overlay_dir/patches/$patch_name" >/dev/null || fail "overlay patch did not apply: $patch_name"
done < <(jq -er '.audit.recipe_metadata[].overlay_patches[]?' "$lock_file")
grep -F "bash_completions = []" "$tmp_dir/recipes/gstreamer-1.0.recipe" >/dev/null || {
    fail "GStreamer bash completions patch did not apply"
}
if grep -Eq "^[[:space:]]*bash_completions[[:space:]]*=.*(gst-inspect-1\\.0|gst-launch-1\\.0)" \
    "$tmp_dir/recipes/gstreamer-1.0.recipe"; then
    fail "GStreamer recipe still lists gst shell completions"
fi

# Keep this friendly AST seam local so discovery can fail independently.
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

facts = {"name": None, "version": None, "url": None, "sha256": None,
         "package_name": None, "tarball_dirname": None, "deps": [], "platform_deps": [],
         "file_patterns": {}, "file_errors": [], "enable_plugin_targets": [],
         "control_errors": [], "meson_enabled": [], "meson_control_errors": []}
allowed_plugin_targets = set()
meson_assignment_seen = False
def file_error(name):
    if name not in facts["file_errors"]:
        facts["file_errors"].append(name)
def control_error(name):
    if name not in facts["control_errors"]:
        facts["control_errors"].append(name)
def meson_error(name):
    if name not in facts["meson_control_errors"]:
        facts["meson_control_errors"].append(name)
def string_value(node):
    return node.value if isinstance(node, ast.Constant) and isinstance(node.value, str) else None
def meson_subscript(node):
    if not isinstance(node, ast.Subscript):
        return None
    owner = node.value
    if not (isinstance(owner, ast.Attribute) and owner.attr == "meson_options"
            and isinstance(owner.value, ast.Name) and owner.value.id == "self"):
        return None
    return node.slice
def record_meson_assignment(target, value_node):
    key = string_value(target)
    if key is None:
        meson_error("meson_options")
        return
    value = string_value(value_node)
    if value is None:
        meson_error("meson_options")
        return
    if value == "enabled" and key not in facts["meson_enabled"]:
        facts["meson_enabled"].append(key)
for node in ast.walk(tree):
    if isinstance(node, ast.Assign):
        for target in node.targets:
            if not isinstance(target, ast.Name):
                continue
            if target.id == "name" and isinstance(node.value, ast.Constant):
                facts["name"] = node.value.value
            elif target.id == "version" and isinstance(node.value, ast.Constant):
                facts["version"] = node.value.value
            elif target.id == "url" and isinstance(node.value, ast.Constant):
                facts["url"] = node.value.value
            elif target.id == "tarball_checksum" and isinstance(node.value, ast.Constant):
                facts["sha256"] = node.value.value
            elif target.id in ("package_name", "tarball_dirname") and isinstance(node.value, ast.Constant):
                facts[target.id] = node.value.value
            elif target.id == "deps":
                facts["deps"] = strings(node.value)
            elif target.id == "platform_deps" and isinstance(node.value, ast.Dict):
                for key, value in zip(node.value.keys, node.value.values):
                    if isinstance(key, ast.Attribute) and key.attr == "LINUX":
                        facts["platform_deps"] = strings(value)
            elif target.id == "meson_options":
                if meson_assignment_seen or not isinstance(node.value, ast.Dict):
                    meson_error("meson_options")
                    continue
                meson_assignment_seen = True
                seen_meson_keys = set()
                for key, value in zip(node.value.keys, node.value.values):
                    key_value = string_value(key)
                    value_value = string_value(value)
                    if key_value is None or value_value is None or key_value in seen_meson_keys:
                        meson_error("meson_options")
                        continue
                    seen_meson_keys.add(key_value)
                    if value_value == "enabled":
                        facts["meson_enabled"].append(key_value)
            elif target.id.startswith("files_plugins_"):
                if len(node.targets) != 1 or not isinstance(node.value, (ast.List, ast.Tuple)):
                    file_error(target.id)
                    continue
                values = []
                for item in node.value.elts:
                    if not isinstance(item, ast.Constant) or not isinstance(item.value, str):
                        file_error(target.id)
                        break
                    values.append(item.value)
                else:
                    if target.id in facts["file_patterns"]:
                        file_error(target.id)
                    else:
                        facts["file_patterns"][target.id] = values
                        allowed_plugin_targets.add(id(target))
for node in ast.walk(tree):
    if isinstance(node, ast.Name) and node.id.startswith("files_plugins_") and id(node) not in allowed_plugin_targets:
        file_error(node.id)
    elif (isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name)
          and node.value.id == "self" and node.attr.startswith("files_plugins_")):
        file_error(node.attr)
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
        owner = node.func.value
        if isinstance(owner, ast.Name) and owner.id == "self":
            if node.func.attr == "enable_plugin":
                target = string_value(node.args[0]) if node.args else None
                if target is None:
                    control_error("enable_plugin")
                elif target not in facts["enable_plugin_targets"]:
                    facts["enable_plugin_targets"].append(target)
            elif node.func.attr == "disable_plugin":
                control_error("disable_plugin")
        elif (isinstance(owner, ast.Attribute) and owner.attr == "meson_options"
              and isinstance(owner.value, ast.Name) and owner.value.id == "self"):
            meson_error("meson_options")
    if isinstance(node, ast.Assign):
        for target in node.targets:
            key = meson_subscript(target)
            if key is not None:
                record_meson_assignment(key, node.value)
    elif isinstance(node, ast.AnnAssign):
        if meson_subscript(node.target) is not None:
            meson_error("meson_options")
    elif isinstance(node, (ast.AugAssign, ast.Delete)):
        targets = node.targets if isinstance(node, ast.Delete) else [node.target]
        if any(meson_subscript(target) is not None for target in targets):
            meson_error("meson_options")
print(json.dumps(facts, sort_keys=True))
PY
}

assert_reviewed_recipe_controls() {
    local recipe=$1 expected_plugins=$2 expected_meson=$3 facts
    facts=$(recipe_facts "$tmp_dir/recipes/$recipe.recipe") || {
        fail "could not parse audited recipe controls: $recipe"
    }
    jq -e --argjson expected_plugins "$expected_plugins" --argjson expected_meson "$expected_meson" '
        (.control_errors == []) and (.meson_control_errors == [])
        and ((.enable_plugin_targets | sort) == ($expected_plugins | sort))
        and ((.meson_enabled | sort) == ($expected_meson | sort))
    ' <<<"$facts" >/dev/null || fail "unreviewed plugin control in recipe: $recipe"
}

assert_reviewed_recipe_controls gstreamer-1.0 '[]' '["libunwind", "ptp-helper"]'
assert_reviewed_recipe_controls gst-plugins-base-1.0 '["alsa"]' '["opus"]'
assert_reviewed_recipe_controls gst-plugins-good-1.0 '["pulseaudio"]' '["adaptivedemux2", "soup", "vpx"]'
assert_reviewed_recipe_controls gst-plugins-bad-1.0 '["va"]' '[]'

# Resolve only the plugin categories named by the package specs. recipe_facts
# is the sole AST seam; this shell layer rejects dynamic/malformed declarations
# and normalizes the one reviewed Cerbero extension pattern to a .so basename.
plugin_set_from_pinned_recipes() {
    local recipes_dir=$1 package_specs=$2 recipe category facts patterns pattern basename
    local pattern_re='^%\(libdir\)s/gstreamer-1\.0/libgst[A-Za-z0-9_+-]+%\(mext\)s$'
    declare -A seen=()
    while IFS=$'\t' read -r recipe category; do
        if ! facts=$(recipe_facts "$recipes_dir/$recipe.recipe"); then
            return 1
        fi
        if ! patterns=$(jq -er --arg key "files_$category" '
            if ((.file_errors | index($key)) != null) then error("dynamic plugin file list")
            elif (.file_patterns[$key] | type) != "array" then error("missing plugin file list")
            else .file_patterns[$key][]
            end
        ' <<<"$facts"); then
            return 1
        fi
        while IFS= read -r pattern; do
            [[ "$pattern" =~ $pattern_re ]] || return 1
            basename=$(sed 's/%(mext)s$/.so/' <<<"${pattern##*/}")
            [[ "$basename" == libgst*.so ]] || return 1
            if [[ -n "${seen[$basename]+x}" ]]; then
                return 1
            fi
            seen["$basename"]=1
            printf '%s\n' "$basename"
        done <<<"$patterns"
    done <"$package_specs"
}

# Parse the pinned archive's actual post-overlay recipe files. This is the
# discovery-side guard against copying guessed URL/version/checksum/dependency
# values into the lock.
while IFS=$'\t' read -r recipe expected_version expected_url expected_sha expected_deps expected_platform expected_archive_root; do
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
    if [[ -n "$expected_archive_root" ]]; then
        actual_name=$(jq -r '.name // empty' <<<"$facts")
        actual_version=${actual_version:-$expected_version}
        actual_package_name=$(jq -r '.package_name // empty' <<<"$facts")
        actual_tarball_dirname=$(jq -r '.tarball_dirname // empty' <<<"$facts")
        [[ -n "$actual_name" ]] || fail "recipe name is missing for archive-root validation: $recipe"
        if [[ -n "$actual_tarball_dirname" ]]; then
            effective_archive_root=${actual_tarball_dirname//%(version)s/$actual_version}
        elif [[ -n "$actual_package_name" ]]; then
            effective_archive_root=$actual_package_name
        else
            effective_archive_root="$actual_name-$actual_version"
        fi
        [[ "$effective_archive_root" == "$expected_archive_root" ]] || fail "recipe archive root disagrees: $recipe"
    fi
    raw_url=$(jq -r '.url // empty' <<<"$facts")
    case "$raw_url" in
        ''|gnome://*|xiph://*|*'%(*)'*) ;;
        *) [[ "$raw_url" == "$expected_url" ]] || fail "recipe URL disagrees: $recipe" ;;
    esac
    jq -e --arg recipe "$recipe" --arg url "$expected_url" --arg sha "$expected_sha" \
        'any(.components[]; .recipe == $recipe and .source_url == $url and .sha256 == $sha)' \
        "$lock_file" >/dev/null || fail "component source metadata disagrees: $recipe"
done < <(jq -er '.audit.recipe_metadata[] | [.recipe, .version, .source_url, .sha256, (.deps | tojson), (.platform_deps | tojson), (.archive_root // "")] | @tsv' "$lock_file")

package_plugin_specs="$tmp_dir/plugin-package-specs"
if ! jq -er '
    .[] | split(":") as $parts
    | $parts[1:][] | select(startswith("plugins_"))
    | [$parts[0], .] | @tsv
' "$overlay_package_files" >"$package_plugin_specs" || [[ ! -s "$package_plugin_specs" ]]; then
    fail "audited package plugin categories are missing or malformed"
fi
declared_plugin_files="$tmp_dir/declared-plugin-files"
if ! plugin_set_from_pinned_recipes "$tmp_dir/recipes" "$package_plugin_specs" >"$declared_plugin_files"; then
    fail "pinned recipe plugin categories are not the exact lock set"
fi
sort -o "$declared_plugin_files" "$declared_plugin_files"
expected_plugin_files="$tmp_dir/expected-plugin-files"
if ! jq -er '[.audit.plugin_allowlist[].filename] | unique | sort[]' "$lock_file" >"$expected_plugin_files"; then
    fail "lock plugin allowlist cannot produce a unique filename set"
fi
if ! cmp -s "$declared_plugin_files" "$expected_plugin_files"; then
    fail "pinned recipe plugin categories differ from the lock allowlist"
fi

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
    --arg pulse_url "$pulse_url" \
    --arg pulse_sha "$pulse_sha" \
    --arg pulse_tag "$pulse_tag" \
    --arg pulse_tag_object "$pulse_tag_object" \
    --arg pulse_tag_commit "$pulse_tag_commit" \
    --arg pulse_signature "$pulse_signature" \
    --arg pulse_archive_root "$pulse_archive_root" \
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
        elif .recipe == "libpulse" then .version = "17.0" | .source_url = $pulse_url | .sha256 = $pulse_sha | .tag = $pulse_tag | .tag_object = $pulse_tag_object | .tag_commit = $pulse_tag_commit | .signature = $pulse_signature
        else . end)
    | .audit.recipe_metadata |= map(
        if .recipe == "gstreamer-1.0" then .version = $version | .source_url = $gstreamer_url | .sha256 = $gstreamer_sha
        elif .recipe == "gst-libav-1.0" then .version = $version | .source_url = $libav_url | .sha256 = $libav_sha
        elif .recipe == "pipewire" then .source_url = $pipewire_url | .sha256 = $pipewire_sha
        elif .recipe == "libpulse" then .version = "17.0" | .source_url = $pulse_url | .sha256 = $pulse_sha | .archive_root = $pulse_archive_root | .tag = $pulse_tag | .tag_object = $pulse_tag_object | .tag_commit = $pulse_tag_commit | .signature = $pulse_signature
        else . end)' "$lock_file" >"$lock_tmp"
mv -f "$lock_tmp" "$lock_file"
trap - EXIT
echo "verified all locked Cerbero recipes and updated official metadata in $lock_file"
