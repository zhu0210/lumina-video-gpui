#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
fixture_root=$(mktemp -d "${TMPDIR:-/tmp}/lumina-shared-library-test.XXXXXX")
cleanup() { rm -rf -- "$fixture_root"; }
trap cleanup EXIT

jq -L "$script_dir" -e 'include "gstreamer-shared-library-manifest"; valid_shared_library_allowlist' \
    "$repo_root/vendor/gstreamer-1.0.lock.json" >/dev/null
jq '.audit.shared_library_allowlist[0].component = "missing-owner"' \
    "$repo_root/vendor/gstreamer-1.0.lock.json" >"$fixture_root/unknown-owner-lock.json"
! jq -L "$script_dir" -e 'include "gstreamer-shared-library-manifest"; valid_shared_library_allowlist' \
    "$fixture_root/unknown-owner-lock.json" >/dev/null

public_dir="$fixture_root/runtime/lib/x86_64-linux-gnu"
private_dir="$public_dir/pulseaudio"
mkdir -p "$private_dir"
printf 'public\n' >"$public_dir/libfixture.so.1"
printf 'debug\n' >"$public_dir/libfixture.so.1.debuginfo"
ln -s libfixture.so.1 "$public_dir/libfixture.so"
printf 'private\n' >"$private_dir/libprivate.so.1"
ln -s libprivate.so.1 "$private_dir/libprivate.so"

inventory="$fixture_root/inventory.json"
python3 "$script_dir/gstreamer-shared-library-inventory.py" \
    "$fixture_root/runtime" lib/x86_64-linux-gnu lib/x86_64-linux-gnu/pulseaudio \
    >"$inventory"
jq -e 'all(.entries[]; .path != "libfixture.so.1.debuginfo")' "$inventory" >/dev/null
printf '%s\n' libfixture.so pulseaudio/libprivate.so | sort >"$fixture_root/expected"
jq -r '.canonical_paths[]' "$inventory" >"$fixture_root/actual"
cmp -s "$fixture_root/expected" "$fixture_root/actual"

cp -- "$fixture_root/actual" "$fixture_root/extra"
printf '%s\n' libextra.so >>"$fixture_root/extra"
! cmp -s "$fixture_root/expected" "$fixture_root/extra"
sed '$d' "$fixture_root/actual" >"$fixture_root/missing"
! cmp -s "$fixture_root/expected" "$fixture_root/missing"

shared_library_prefix='vendor/linux-x86_64/lib/x86_64-linux-gnu/'
jq --arg prefix "$shared_library_prefix" '
    {bundled_files: [.entries[]
        | {kind, path: ($prefix + .path), component: "fixture"}
          + if .kind == "symlink" then {link_target} else {} end]}
' "$inventory" >"$fixture_root/manifest"
expected_entries=$(jq -c '[.entries[] | {path, kind, link_target: (.link_target // null)}] | sort_by(.path)' "$inventory")
manifest_entries=$(jq -L "$script_dir" -c --arg prefix "$shared_library_prefix" \
    'include "gstreamer-shared-library-manifest"; .bundled_files | shared_library_entries($prefix)' \
    "$fixture_root/manifest")
[[ "$manifest_entries" == "$expected_entries" ]]
expected_owners=$(jq -c '[.canonical_paths[] | {component: "fixture", path: .}] | sort_by(.path)' "$inventory")
manifest_owners=$(jq -L "$script_dir" -c --arg prefix "$shared_library_prefix" \
    'include "gstreamer-shared-library-manifest"; .bundled_files | shared_library_owners($prefix)' \
    "$fixture_root/manifest")
[[ "$manifest_owners" == "$expected_owners" ]]
jq '(.bundled_files[] | select(.path | contains("libfixture"))).component = "wrong-owner"' \
    "$fixture_root/manifest" >"$fixture_root/wrong-owner"
wrong_owners=$(jq -L "$script_dir" -c --arg prefix "$shared_library_prefix" \
    'include "gstreamer-shared-library-manifest"; .bundled_files | shared_library_owners($prefix)' \
    "$fixture_root/wrong-owner")
[[ "$wrong_owners" != "$expected_owners" ]]

# A same-canonical alias does not change canonical_paths, so the exact entry
# projection must independently catch it when the manifest omits the alias.
ln -s libfixture.so.1 "$public_dir/libfixture.so.999"
python3 "$script_dir/gstreamer-shared-library-inventory.py" \
    "$fixture_root/runtime" lib/x86_64-linux-gnu lib/x86_64-linux-gnu/pulseaudio \
    >"$fixture_root/extra-alias.json"
extra_alias_entries=$(jq -c '[.entries[] | {path, kind, link_target: (.link_target // null)}] | sort_by(.path)' \
    "$fixture_root/extra-alias.json")
[[ "$manifest_entries" != "$extra_alias_entries" ]]
unlink "$public_dir/libfixture.so.999"

ln -s /etc/passwd "$public_dir/libescape.so"
! python3 "$script_dir/gstreamer-shared-library-inventory.py" \
    "$fixture_root/runtime" lib/x86_64-linux-gnu lib/x86_64-linux-gnu/pulseaudio \
    >"$fixture_root/escape.json" 2>/dev/null
unlink "$public_dir/libescape.so"
ln -s ../liboutside.so.1 "$public_dir/librelativeescape.so"
! python3 "$script_dir/gstreamer-shared-library-inventory.py" \
    "$fixture_root/runtime" lib/x86_64-linux-gnu lib/x86_64-linux-gnu/pulseaudio \
    >"$fixture_root/relative-escape.json" 2>/dev/null
unlink "$public_dir/librelativeescape.so"
ln -s missing.so.1 "$public_dir/libdangling.so"
! python3 "$script_dir/gstreamer-shared-library-inventory.py" \
    "$fixture_root/runtime" lib/x86_64-linux-gnu lib/x86_64-linux-gnu/pulseaudio \
    >"$fixture_root/dangling.json" 2>/dev/null
unlink "$public_dir/libdangling.so"
printf 'other\n' >"$public_dir/libother.so.1"
ln -s libother.so.1 "$public_dir/libcross.so"
! python3 "$script_dir/gstreamer-shared-library-inventory.py" \
    "$fixture_root/runtime" lib/x86_64-linux-gnu lib/x86_64-linux-gnu/pulseaudio \
    >"$fixture_root/cross-name.json" 2>/dev/null
unlink "$public_dir/libcross.so"
unlink "$public_dir/libother.so.1"
printf 'duplicate\n' >"$public_dir/libfixture.so.2"
! python3 "$script_dir/gstreamer-shared-library-inventory.py" \
    "$fixture_root/runtime" lib/x86_64-linux-gnu lib/x86_64-linux-gnu/pulseaudio \
    >"$fixture_root/duplicate.json" 2>/dev/null
unlink "$public_dir/libfixture.so.2"
mkfifo "$public_dir/libspecial.so"
! python3 "$script_dir/gstreamer-shared-library-inventory.py" \
    "$fixture_root/runtime" lib/x86_64-linux-gnu lib/x86_64-linux-gnu/pulseaudio \
    >"$fixture_root/special.json" 2>/dev/null
unlink "$public_dir/libspecial.so"

# Execute both standalone recipe_facts parsers against the same fixtures; do
# not maintain a third parser in this test.
printf '%s\n' "files_libs = ['libfixture']" >"$fixture_root/literal.recipe"
printf '%s\n' "files_libs = ['libfixture']" "files_libs.append('libextra')" >"$fixture_root/dynamic.recipe"
printf '%s\n' \
    "files_libs = ['liborc-0.4']" \
    "files_libs_lumina = ['liborc-0.4']" \
    "class Recipe:" \
    "    def prepare(self):" \
    "        self.files_libs.append('liborc-test-0.4')" \
    >"$fixture_root/orc.recipe"
for parser_script in build-gstreamer-runtime.sh discover-gstreamer-lock.sh; do
    awk '
        /^recipe_facts\(\) \{/ { in_function = 1 }
        in_function && /<<'\''PY'\''$/ { capture = 1; next }
        capture && /^PY$/ { exit }
        capture { print }
    ' "$script_dir/$parser_script" >"$fixture_root/recipe-facts.py"
    python3 "$fixture_root/recipe-facts.py" "$fixture_root/literal.recipe" >"$fixture_root/literal.json"
    python3 "$fixture_root/recipe-facts.py" "$fixture_root/dynamic.recipe" >"$fixture_root/dynamic.json"
    python3 "$fixture_root/recipe-facts.py" "$fixture_root/orc.recipe" >"$fixture_root/orc.json"
    jq -e '.file_patterns.files_libs == ["libfixture"] and .file_errors == []' \
        "$fixture_root/literal.json" >/dev/null
    jq -e '.file_errors | index("files_libs") != null' "$fixture_root/dynamic.json" >/dev/null
    jq -e '
        .file_errors == []
        and .file_patterns.files_libs == ["liborc-0.4", "liborc-test-0.4"]
        and .file_patterns.files_libs_lumina == ["liborc-0.4"]
    ' "$fixture_root/orc.json" >/dev/null
done
