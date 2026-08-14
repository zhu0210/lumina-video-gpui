#!/usr/bin/env bash
set -euo pipefail

# Static post-build audit. It deliberately re-runs the plugin metadata check
# from the extracted artifact; the lock's declared license is not evidence.
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
artifact=${1:-}
source_artifact=${2:-}
lock_file=${3:-$repo_root/vendor/gstreamer-1.0.lock.json}

[[ -n "$artifact" && -n "$source_artifact" ]] || {
    echo "Usage: $0 RUNTIME.tar.xz SOURCES.tar.xz [LOCK]" >&2
    exit 2
}
for command_name in jq tar sha256sum mktemp realpath grep awk sed find readelf tr sort; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done
artifact=$(realpath "$artifact")
source_artifact=$(realpath "$source_artifact")
lock_file=$(realpath "$lock_file")
[[ -f "$artifact" && -f "$source_artifact" && -f "$lock_file" ]] || exit 1

fail() {
    echo "audit-gstreamer-runtime: $*" >&2
    exit 1
}

schema=$(jq -er '.schema_version' "$lock_file")
[[ "$schema" == 2 ]] || fail "unsupported audit lock schema"
[[ "$(jq -c '.packages' "$lock_file")" == '["lumina-audited"]' ]] || fail "lock package is not lumina-audited"
jq -e '(.audit.recipe_allowlist | length == 23) and (all(.audit.recipe_allowlist[]; . != "lumina-audited"))' "$lock_file" >/dev/null || {
    fail "recipe allowlist contains the custom package or has an unexpected closure"
}

# Stream tar listing directly. Keeping the complete member list in a shell
# variable made archive safety proportional to archive size.
for member_archive in "$artifact" "$source_artifact"; do
    tar -tJf "$member_archive" | while IFS= read -r member; do
        member=${member#./}
        case "$member" in
            /*|.|..|*/.|*/./*|*/..|*/../*|../*|*//* )
                echo "unsafe archive member: $member" >&2
                exit 1
                ;;
        esac
    done
done
if tar -tJf "$artifact" | grep -Eiq 'x264|gst-plugins-ugly|nonfree'; then
    fail "forbidden licensing/component name in runtime archive"
fi

manifest=$(tar -xJOf "$artifact" ./runtime-manifest.json)
jq -e \
    '.schema_version == 2 and (.packages == ["lumina-audited"]) and (.package_files | length >= 7) and .policy.gpl == false and .policy.nonfree == false and .policy.unknown == false and .policy.ugly == false and (.variants == ["norust", "alsa", "pulse", "va"]) and (.recursive_dt_needed | type == "array") and (.bundled_files | type == "array") and (.components | type == "array") and (.actual_plugin_license | type == "array")' \
    <<<"$manifest" >/dev/null || fail "runtime manifest policy or inventory is incomplete"
jq -e --argjson expected "$(jq -c '.audit.package_files' "$lock_file")" \
    '.package_files == $expected' <<<"$manifest" >/dev/null || fail "runtime manifest package file categories differ from lock"
jq -e --argjson expected "$(jq -c '.components' "$lock_file")" \
    '.components == $expected' <<<"$manifest" >/dev/null || fail "runtime manifest component inventory differs from lock"
jq -e --argjson owners "$(jq -c '[.components[].name]' "$lock_file")" \
    'all(.actual_plugin_license[]; (.component as $owner | (.filename and .license and .source_url and ($owners | index($owner) != null))))' \
    <<<"$manifest" >/dev/null || {
    fail "runtime manifest effective plugin inventory is incomplete"
}
jq -e --argjson owners "$(jq -c '[.components[].name]' "$lock_file")" \
    'all(.bundled_files[] | select(.path | test("\\.so($|\\.)")); .component as $owner | ($owners | index($owner)))' \
    <<<"$manifest" >/dev/null || fail "runtime shared-library ownership is incomplete"

runtime_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-runtime-audit.XXXXXX")
source_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-source-audit.XXXXXX")
cache_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-plugin-cache.XXXXXX")
cleanup() { rm -rf "$runtime_dir" "$source_dir" "$cache_dir"; }
trap cleanup EXIT
tar -xJf "$artifact" -C "$runtime_dir" --no-same-owner
tar -xJf "$source_artifact" -C "$source_dir" --no-same-owner

while IFS=$'\t' read -r path expected owner; do
    [[ -f "$runtime_dir/$path" ]] || fail "manifest file is missing: $path"
    actual=$(sha256sum "$runtime_dir/$path" | awk '{print $1}')
    [[ "$actual" == "$expected" ]] || fail "manifest hash mismatch: $path"
    if [[ "$path" == *.so || "$path" == *.so.* ]]; then
        [[ -n "$owner" && "$owner" != metadata ]] || fail "shared library lacks component owner: $path"
    fi
done < <(jq -r '.bundled_files[] | [.path, .sha256, (.component // "")] | @tsv' <<<"$manifest")

source_manifest=$(jq -e . "$source_dir/source-manifest.json")
[[ "$(jq -r '.runtime_tree_sha256' <<<"$source_manifest")" == "$(jq -r '.tree_sha256' <<<"$manifest")" ]] || {
    fail "source bundle does not correspond to runtime tree"
}
jq -e --argjson expected "$(jq -c '.components' "$lock_file")" \
    '(.components | map({name, version, source_url, sha256, cache_path})) == ($expected | map({name, version, source_url, sha256, cache_path}))' \
    <<<"$source_manifest" >/dev/null || fail "source manifest component inventory differs from lock"
jq -e --argjson expected "$(jq -c '[.components[] | {name, version, source_url, sha256, cache_path}]' "$lock_file")" \
    '.archives == $expected' <<<"$source_manifest" >/dev/null || fail "source manifest archive inventory differs from lock"
cerbero_source_rel=$(jq -er '.cerbero_archive.path' <<<"$source_manifest")
case "$cerbero_source_rel" in
    /*|../*|*/../*|*//* ) fail "source manifest Cerbero path escapes source root" ;;
esac
cerbero_source_path="$source_dir/$cerbero_source_rel"
[[ -f "$cerbero_source_path" ]] || fail "corresponding source lacks the pinned Cerbero archive"
cerbero_source_sha=$(jq -er '.cerbero_archive.sha256' <<<"$source_manifest")
printf '%s  %s\n' "$cerbero_source_sha" "$cerbero_source_path" | sha256sum -c - >/dev/null || {
    fail "pinned Cerbero archive hash mismatch"
}
jq -e --arg url "$(jq -er '.cerbero.archive.url' "$lock_file")" \
    --arg sha "$(jq -er '.cerbero.archive.sha256' "$lock_file")" \
    '.cerbero_archive.source_url == $url and .cerbero_archive.sha256 == $sha' <<<"$source_manifest" >/dev/null || {
    fail "source manifest Cerbero archive metadata differs from lock"
}

while IFS=$'\t' read -r name version url sha cache_path; do
    source_path="$source_dir/archives/$cache_path"
    [[ -f "$source_path" ]] || fail "corresponding source archive is missing: $name"
    printf '%s  %s\n' "$sha" "$source_path" | sha256sum -c - >/dev/null || {
        fail "corresponding source archive hash mismatch: $name"
    }
done < <(jq -er '.components[] | [.name, .version, .source_url, .sha256, .cache_path] | @tsv' "$lock_file")
[[ -d "$source_dir/overlay/patches" &&
   -f "$source_dir/overlay/patches/gst-plugins-bad-1.0-disable-gpl.patch" &&
   -f "$source_dir/overlay/patches/gst-plugins-bad-1.0-no-gpl-deps.patch" &&
   -f "$source_dir/overlay/patches/gst-plugins-bad-1.0-minimal.patch" &&
   -f "$source_dir/overlay/patches/gst-plugins-base-1.0-minimal.patch" &&
   -f "$source_dir/overlay/patches/gst-plugins-good-1.0-minimal.patch" ]] || {
    fail "source bundle lacks the applied recipe patches"
}
[[ -f "$source_dir/overlay/recipes/pipewire.recipe" && -f "$source_dir/overlay/packages/lumina-audited.package" ]] || {
    fail "source bundle lacks the custom PipeWire/package recipes"
}
jq -e '
    .sources.pipewire.plugin_license == "MIT/X11" and
    .sources.pipewire.plugin_license_source == "src/gst" and
    any(.audit.plugin_allowlist[]; .element == "pipewiresink" and .source == "pipewire/src/gst" and .license == "MIT/X11")
' "$lock_file" >/dev/null || fail "lock PipeWire plugin provenance is incomplete"

runtime_root="$runtime_dir/vendor/linux-x86_64"
runtime_libdir=$(jq -er '.artifact.archive_layout.runtime_libdir' "$lock_file")
runtime_plugin_dir="$runtime_root/$runtime_libdir/gstreamer-1.0"
runtime_bin="$runtime_root/bin"
scanner="$runtime_root/libexec/gstreamer-1.0/gst-plugin-scanner"
[[ -d "$runtime_plugin_dir" && -x "$runtime_bin/gst-inspect-1.0" && -x "$scanner" ]] || fail "runtime inspection tools are missing"
export HOME="$cache_dir/home" XDG_CACHE_HOME="$cache_dir"
mkdir -p "$HOME"
export LD_LIBRARY_PATH="$runtime_root/$runtime_libdir"
export GST_PLUGIN_PATH_1_0="$runtime_plugin_dir"
export GST_PLUGIN_SYSTEM_PATH_1_0=
export GST_PLUGIN_PATH=
export GST_PLUGIN_SYSTEM_PATH=
export GST_PLUGIN_SCANNER_1_0="$scanner"
export GST_PLUGIN_SCANNER=
export GST_REGISTRY_1_0="$cache_dir/gstreamer-1.0.registry"
export GST_REGISTRY=
export GST_REGISTRY_REUSE_PLUGIN_SCANNER=no

inspect_license() {
    local plugin_path=$1 plugin_name=$2 report license normalized
    report=$("$runtime_bin/gst-inspect-1.0" "$plugin_name" 2>/dev/null) || fail "gst-inspect failed for $plugin_path"
    license=$(sed -n 's/^[[:space:]]*License:[[:space:]]*//p' <<<"$report" | sed -n '1p')
    normalized=$(tr '[:upper:]' '[:lower:]' <<<"$license")
    [[ -n "$license" && "$normalized" != unknown* ]] || fail "unknown plugin license: $plugin_path"
    [[ "${normalized//lgpl/}" != *gpl* ]] || fail "GPL plugin license: $plugin_path ($license)"
}
while IFS= read -r -d '' plugin_path; do
    plugin_file=${plugin_path#"$runtime_plugin_dir"/}
    plugin_name=${plugin_file#libgst}
    plugin_name=${plugin_name%%.so*}
    inspect_license "$plugin_file" "$plugin_name"
    jq -e --arg path "vendor/linux-x86_64/$runtime_libdir/gstreamer-1.0/$plugin_file" \
        'any(.bundled_files[]; .path == $path and ((.component // "") != ""))' <<<"$manifest" >/dev/null || {
        fail "bundled plugin is not owned by a packaged component: $plugin_file"
    }
done < <(find -P "$runtime_plugin_dir" -type f -name 'libgst*.so*' -print0 | sort -z)

while IFS=$'\t' read -r element filename owner declared; do
    [[ -f "$runtime_plugin_dir/$filename" ]] || fail "audited plugin file is missing: $filename"
    report=$("$runtime_bin/gst-inspect-1.0" "$element" 2>/dev/null) || fail "audited element is not resolvable: $element"
    actual=$(sed -n 's/^[[:space:]]*License:[[:space:]]*//p' <<<"$report" | sed -n '1p')
    [[ -n "$actual" ]] || fail "audited element has no effective license: $element"
    if [[ "$owner" == PipeWire ]]; then
        [[ "$actual" == *MIT* || "$actual" == *X11* ]] || fail "PipeWire plugin is not MIT/X11: $actual"
    fi
done < <(jq -er '.audit.plugin_allowlist[] | [.element, .filename, .owner_component, .license] | @tsv' "$lock_file")

# The allowlist is a ceiling, not a claim that every GPU/display ABI is used by
# every build. The closure check below rejects any external name outside it.
jq -e --argjson allowed "$(jq -c '.audit.system_elf_allowlist' "$lock_file")" \
    'all(.recursive_dt_needed[] | select(.scope == "system-abi"); (.needed | IN($allowed[])))' <<<"$manifest" >/dev/null || {
    fail "recursive closure contains an unapproved external ABI"
}

while IFS= read -r license_path; do
    [[ -s "$runtime_dir/$license_path" ]] || fail "runtime license text is missing: $license_path"
done < <(jq -er '.license_texts[]' "$lock_file")

echo "audited runtime and exact corresponding source archive passed"
