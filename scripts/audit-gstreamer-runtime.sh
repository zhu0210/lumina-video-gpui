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
for command_name in jq tar sha256sum mktemp realpath grep awk sed find readelf tr sort cmp comm head; do
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
jq -e '
    (.audit.recipe_allowlist | sort | unique) == ([.components[].recipe] | sort | unique) and
    (all(.audit.recipe_allowlist[]; . != "lumina-audited")) and
    (.audit.recipe_metadata | map(.recipe) | sort | unique) == (.audit.recipe_allowlist | sort | unique)
' "$lock_file" >/dev/null || {
    fail "recipe allowlist/metadata must equal the unique component recipe closure"
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
    '.schema_version == 2 and (.packages == ["lumina-audited"]) and (.package_files | length >= 7) and .policy.gpl == false and .policy.nonfree == false and .policy.unknown == false and .policy.ugly == false and (.variants == ["norust", "nogi", "nounwind", "alsa", "pulse", "va"]) and (.recursive_dt_needed | type == "array") and (.bundled_files | type == "array") and (.components | type == "array") and (.actual_plugin_license | type == "array")' \
    <<<"$manifest" >/dev/null || fail "runtime manifest policy or inventory is incomplete"
jq -e --argjson expected "$(jq -c '.audit.package_files' "$lock_file")" \
    '.package_files == $expected' <<<"$manifest" >/dev/null || fail "runtime manifest package file categories differ from lock"
jq -e --argjson expected "$(jq -c '.components' "$lock_file")" \
    '.components == $expected' <<<"$manifest" >/dev/null || fail "runtime manifest component inventory differs from lock"
if ! plugin_allowlist_json=$(jq -e -c '.audit.plugin_allowlist | select(type == "array")' "$lock_file"); then
    fail "lock plugin allowlist is not an array"
fi
jq -e --argjson expected "$plugin_allowlist_json" \
    '.plugin_effective_license == $expected' <<<"$manifest" >/dev/null || fail "runtime manifest plugin allowlist differs from lock"
jq -e --argjson owners "$(jq -c '[.components[].name]' "$lock_file")" \
    'all(.actual_plugin_license[]; (type == "object" and (keys | sort) == ["component", "filename", "license", "source_module", "source_url"] and .filename and .license and .component and .source_module and .source_url and (.component as $owner | ($owners | index($owner) != null))))' \
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
    '(.components | map({name, recipe, version, source_url, sha256})) == ($expected | map({name, recipe, version, source_url, sha256}))' \
    <<<"$source_manifest" >/dev/null || fail "source manifest component inventory differs from lock"
jq -e --argjson expected "$(jq -c '[.components[] | . as $component | {name, recipe, version, source_url, sha256, filename: (.source_url | split("/") | last), path: ("archives/" + .recipe + "/" + (.source_url | split("/") | last))}]' "$lock_file")" \
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

while IFS=$'\t' read -r name recipe version url sha filename source_rel; do
    case "$source_rel" in
        archives/*/*) ;;
        *) fail "corresponding source path is not normalized: $source_rel" ;;
    esac
    source_path="$source_dir/$source_rel"
    [[ -f "$source_path" ]] || fail "corresponding source archive is missing: $name"
    printf '%s  %s\n' "$sha" "$source_path" | sha256sum -c - >/dev/null || {
        fail "corresponding source archive hash mismatch: $name"
    }
done < <(jq -er '.components[] | . as $component |
    [$component.name, $component.recipe, $component.version, $component.source_url,
     $component.sha256, ($component.source_url | split("/") | last),
     ("archives/" + $component.recipe + "/" + ($component.source_url | split("/") | last))] | @tsv' "$lock_file")
expected_source_paths="$source_dir/expected-source-paths"
actual_source_paths="$source_dir/actual-source-paths"
jq -er '.archives[].path' <<<"$source_manifest" | sort >"$expected_source_paths"
find -P "$source_dir/archives" -type f -print | sed "s#^$source_dir/##" | sort >"$actual_source_paths"
cmp -s "$expected_source_paths" "$actual_source_paths" || {
    fail "corresponding source contains an extra or missing runtime archive"
}
overlay_patch_dir="$source_dir/overlay/patches"
[[ -d "$overlay_patch_dir" ]] || fail "source bundle lacks the overlay patch directory"
patch_list="$cache_dir/overlay-patches.list"
if ! jq -er '
    [.audit.recipe_metadata[] | select(has("overlay_patches")) | .overlay_patches] as $declared
    | if ($declared | length) == 0 then
          error("no recipe declares overlay_patches")
      elif any($declared[]; type != "array") then
          error("overlay_patches must be arrays")
      else
          [$declared[] | .[]] as $patches
          | if ($patches | length) == 0 then
                error("overlay_patches must not be empty")
            elif any($patches[]; if type == "string" then length == 0 else true end) then
                error("overlay_patches entries must be nonempty strings")
            elif ($patches | unique | length) != ($patches | length) then
                error("overlay_patches entries must be unique")
            else
                $patches[]
            end
      end
' "$lock_file" >"$patch_list"; then
    fail "lock overlay patch metadata is invalid"
fi
while IFS= read -r patch_name; do
    [[ -n "$patch_name" ]] || {
        printf 'audit-gstreamer-runtime: invalid lock overlay patch name %q\n' "$patch_name" >&2
        exit 1
    }
    case "$patch_name" in
        /*|.|..|*/*)
            printf 'audit-gstreamer-runtime: invalid lock overlay patch name %q\n' "$patch_name" >&2
            exit 1
            ;;
    esac
    printf -v patch_display '%q' "$patch_name"
    [[ -f "$overlay_patch_dir/$patch_name" ]] || {
        fail "source bundle lacks lock-listed overlay patch: $patch_display"
    }
done <"$patch_list"
[[ -f "$source_dir/overlay/packages/lumina-audited.package" ]] || {
    fail "source bundle lacks the custom audited package recipe"
}
while IFS= read -r overlay_recipe; do
    [[ -f "$source_dir/overlay/recipes/$overlay_recipe.recipe" ]] || {
        fail "source bundle lacks overlay recipe: $overlay_recipe"
    }
done < <(jq -er '.audit.recipe_metadata[] | select(.overlay == true) | .recipe' "$lock_file")
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
expected_plugin_files="$cache_dir/expected-plugin-files"
if ! jq -er '[.audit.plugin_allowlist[].filename] | unique | sort[]' "$lock_file" >"$expected_plugin_files"; then
    fail "lock plugin allowlist cannot produce a unique filename set"
fi
actual_plugin_details="$cache_dir/runtime-plugin-files.tsv"
if ! find -P "$runtime_plugin_dir" -type f -name 'libgst*.so*' -printf '%f\t%p\n' >"$actual_plugin_details"; then
    fail "cannot enumerate runtime GStreamer plugins"
fi
actual_plugin_files="$cache_dir/runtime-plugin-files"
if ! awk -F '\t' '{ if (seen[$1]++) { print "duplicate runtime plugin filename: " $1 > "/dev/stderr"; bad=1 } print $1 } END { exit bad }' \
    "$actual_plugin_details" >"$actual_plugin_files"; then
    fail "runtime GStreamer plugin filenames are ambiguous"
fi
sort -o "$actual_plugin_files" "$actual_plugin_files"
if ! cmp -s "$actual_plugin_files" "$expected_plugin_files"; then
    plugin_set_diff="$cache_dir/runtime-plugin-set.diff"
    comm -3 "$expected_plugin_files" "$actual_plugin_files" >"$plugin_set_diff"
    head -20 "$plugin_set_diff" >&2
    fail "runtime GStreamer plugin files differ from the lock allowlist"
fi
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

plugin_license_tsv="$cache_dir/plugin-effective-license.tsv"
: >"$plugin_license_tsv"
plugin_metadata_field() {
    local field=$1 report=$2
    sed -n "s/^[[:space:]]*${field}[[:space:]]\\{1,\\}//p" <<<"$report" | sed -n '1p'
}
normalize_plugin_license() {
    case "$1" in
        LGPL) printf '%s\n' 'LGPL-2.1-or-later' ;;
        MIT/X11) printf '%s\n' 'MIT/X11' ;;
        *) fail "unsupported plugin license: $2 ($1)" ;;
    esac
}
inspect_plugin_license() {
    local plugin_path=$1 plugin_file=$2 report raw_license license source_module
    local owner expected_source_module component_license source_url manifest_path
    manifest_path="vendor/linux-x86_64/$runtime_libdir/gstreamer-1.0/$plugin_file"
    owner=$(jq -r --arg path "$manifest_path" '
        first(.file_ownership[] | select(any(.prefixes[]; . as $prefix | ($path | startswith($prefix)))) | .component) // empty
    ' "$lock_file")
    [[ -n "$owner" ]] || fail "bundled plugin has no package/component owner: $plugin_file"
    component_license=$(jq -r --arg owner "$owner" 'first(.components[] | select(.name == $owner) | .license) // empty' "$lock_file")
    source_url=$(jq -r --arg owner "$owner" 'first(.components[] | select(.name == $owner) | .source_url) // empty' "$lock_file")
    [[ -n "$component_license" && -n "$source_url" ]] || fail "bundled plugin owner metadata is incomplete: $plugin_file"
    report=$("$runtime_bin/gst-inspect-1.0" "$plugin_path" 2>/dev/null) || fail "gst-inspect failed for $plugin_file"
    raw_license=$(plugin_metadata_field 'License' "$report")
    source_module=$(plugin_metadata_field 'Source module' "$report")
    [[ -n "$raw_license" && -n "$source_module" ]] || fail "bundled plugin metadata is incomplete: $plugin_file"
    license=$(normalize_plugin_license "$raw_license" "$plugin_file")
    expected_source_module=${owner,,}
    [[ "$source_module" == "$expected_source_module" ]] || {
        fail "bundled plugin source module disagrees with owner: $plugin_file ($source_module != $expected_source_module)"
    }
    if [[ "$license" == LGPL-2.1-or-later* ]]; then
        [[ "$component_license" == LGPL-2.1-or-later* ]] || fail "LGPL plugin owner has incompatible component license: $plugin_file"
    else
        [[ "$component_license" == MIT ]] || fail "MIT/X11 plugin owner has incompatible component license: $plugin_file"
    fi
    jq -e --arg filename "$plugin_file" --arg license "$license" --arg component "$owner" \
        --arg source_module "$source_module" --arg source_url "$source_url" \
        'any(.actual_plugin_license[]; .filename == $filename and .license == $license and .component == $component and .source_module == $source_module and .source_url == $source_url)' \
        <<<"$manifest" >/dev/null || fail "manifest plugin metadata disagrees with inspection: $plugin_file"
    printf '%s\t%s\t%s\t%s\t%s\n' "$plugin_file" "$license" "$owner" "$source_module" "$source_url" >>"$plugin_license_tsv"
}
while IFS= read -r -d '' plugin_path; do
    plugin_file=${plugin_path#"$runtime_plugin_dir"/}
    inspect_plugin_license "$plugin_path" "$plugin_file"
    jq -e --arg path "vendor/linux-x86_64/$runtime_libdir/gstreamer-1.0/$plugin_file" \
        'any(.bundled_files[]; .path == $path and ((.component // "") != ""))' <<<"$manifest" >/dev/null || {
        fail "bundled plugin is not owned by a packaged component: $plugin_file"
    }
done < <(find -P "$runtime_plugin_dir" -type f -name 'libgst*.so*' -print0 | sort -z)

if ! inspected_plugin_inventory=$(jq -Rnc '
    [inputs | select(length > 0) | split("\t")] as $rows
    | if any($rows[]; (length != 5 or any(.[]; . == ""))) then
          error("plugin inspection TSV is not five nonempty fields")
      else
          [$rows[] | {filename: .[0], license: .[1], component: .[2], source_module: .[3], source_url: .[4]}]
          | sort_by([.filename, .component, .source_module, .license, .source_url])
      end
' <"$plugin_license_tsv"); then
    fail "plugin inspection inventory is malformed"
fi
if ! manifest_plugin_inventory=$(jq -c '
    if (all(.actual_plugin_license[]; type == "object") | not) then
        error("manifest plugin inventory contains a non-object")
    elif any(.actual_plugin_license[]; (keys | sort) != ["component", "filename", "license", "source_module", "source_url"]) then
        error("manifest plugin inventory has unexpected fields")
    elif any(.actual_plugin_license[]; (all([.filename, .license, .component, .source_module, .source_url][]; type == "string" and length > 0) | not)) then
        error("manifest plugin inventory has an empty field")
    else
        [.actual_plugin_license[] | {filename: .filename, license: .license, component: .component, source_module: .source_module, source_url: .source_url}]
        | sort_by([.filename, .component, .source_module, .license, .source_url])
    end
' <<<"$manifest"); then
    fail "runtime manifest plugin inventory is malformed"
fi
[[ "$inspected_plugin_inventory" == "$manifest_plugin_inventory" ]] || fail "runtime manifest plugin inventory differs from inspected plugins"

while IFS=$'\t' read -r element filename owner declared; do
    [[ -f "$runtime_plugin_dir/$filename" ]] || fail "audited plugin file is missing: $filename"
    report=$("$runtime_bin/gst-inspect-1.0" "$element" 2>/dev/null) || fail "audited element is not resolvable: $element"
    actual_license=$(awk -F '\t' -v file="$filename" '$1 == file {print $2; exit}' "$plugin_license_tsv")
    actual_owner=$(awk -F '\t' -v file="$filename" '$1 == file {print $3; exit}' "$plugin_license_tsv")
    actual_source_module=$(awk -F '\t' -v file="$filename" '$1 == file {print $4; exit}' "$plugin_license_tsv")
    [[ "$actual_license" == "$declared" ]] || fail "audited element license disagrees with lock: $element"
    [[ "$actual_owner" == "$owner" ]] || fail "audited element owner disagrees with lock: $element"
    [[ "$actual_source_module" == "${owner,,}" ]] || fail "audited element source module disagrees with owner: $element"
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
