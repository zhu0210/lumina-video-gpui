#!/usr/bin/env bash
set -euo pipefail

# Formal build: every version, URL, checksum, package, and variant comes from
# the lock. This script never discovers moving upstream metadata.

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
lock_file="$repo_root/vendor/gstreamer-1.0.lock.json"
output_dir="$repo_root/dist/gstreamer-runtime"

usage() {
    echo "Usage: $0 [--lock PATH] [--output DIR]" >&2
    exit 2
}

while (($#)); do
    case "$1" in
        --lock)
            (($# >= 2)) || usage
            lock_file=$2
            shift 2
            ;;
        --output)
            (($# >= 2)) || usage
            output_dir=$2
            shift 2
            ;;
        *) usage ;;
    esac
done

for command_name in curl jq sha256sum tar xz gzip find sort awk grep sed mktemp realpath chmod cmp cp readlink stat; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done

fail() {
    echo "build-gstreamer-runtime: $*" >&2
    exit 1
}

capture_path() {
    local result_var=$1
    local description=$2
    local marker=$'\x1fLUMINA_PATH_STATUS_'
    local captured status
    shift 2

    captured=$(
        set +e
        "$@"
        command_status=$?
        printf '%s%s' "$marker" "$command_status"
    )
    case "$captured" in
        *"$marker"*)
            status=${captured##*"$marker"}
            captured=${captured%"$marker$status"}
            ;;
        *)
            fail "$description produced no status marker"
            ;;
    esac
    [[ "$status" =~ ^[0-9]+$ ]] || fail "$description produced an invalid status"
    [[ "$status" == 0 ]] || fail "$description failed with status $status"
    [[ "$captured" == *$'\n' ]] || fail "$description produced no terminating newline"
    captured=${captured%$'\n'}
    printf -v "$result_var" '%s' "$captured"
}

lock_file=$(realpath "$lock_file")
output_dir=$(realpath -m "$output_dir")

if jq -e '.. | strings | select(test("latest"; "i"))' "$lock_file" >/dev/null; then
    echo "lock contains moving latest metadata" >&2
    exit 1
fi

gstreamer_version=$(jq -er '.gstreamer.version' "$lock_file")
gstreamer_url=$(jq -er '.gstreamer.source.url' "$lock_file")
gstreamer_sha=$(jq -er '.gstreamer.source.sha256' "$lock_file")
libav_package=$(jq -er '.sources.gst_libav.package' "$lock_file")
libav_filename=$(jq -er '.sources.gst_libav.filename' "$lock_file")
libav_url=$(jq -er '.sources.gst_libav.url' "$lock_file")
libav_sha=$(jq -er '.sources.gst_libav.sha256' "$lock_file")
zlib_version=$(jq -er '.sources.zlib.version' "$lock_file")
zlib_filename=$(jq -er '.sources.zlib.filename' "$lock_file")
zlib_url=$(jq -er '.sources.zlib.url' "$lock_file")
zlib_sha=$(jq -er '.sources.zlib.sha256' "$lock_file")
cerbero_tag=$(jq -er '.cerbero.tag' "$lock_file")
cerbero_tag_object=$(jq -er '.cerbero.tag_object' "$lock_file")
cerbero_commit=$(jq -er '.cerbero.commit' "$lock_file")
cerbero_archive_url=$(jq -er '.cerbero.archive.url' "$lock_file")
cerbero_archive_sha=$(jq -er '.cerbero.archive.sha256' "$lock_file")
builder_image=$(jq -er '.builder.image' "$lock_file")
builder_platform=$(jq -er '.builder.platform' "$lock_file")
schema_version=$(jq -er '.schema_version' "$lock_file")
target_os=$(jq -er '.target.os' "$lock_file")
target_architecture=$(jq -er '.target.architecture' "$lock_file")
target_distribution=$(jq -er '.target.distribution' "$lock_file")
target_distribution_version=$(jq -er '.target.distribution_version' "$lock_file")
target_glibc=$(jq -er '.target.glibc' "$lock_file")
artifact_type=$(jq -er '.artifact.type' "$lock_file")
artifact_compression=$(jq -er '.artifact.compression' "$lock_file")
artifact_split=$(jq -r '.artifact.split' "$lock_file")
mapfile -t packages < <(jq -er '.packages[]' "$lock_file")
mapfile -t variants < <(jq -er '.variants[]' "$lock_file")

[[ "$schema_version" == 1 ]] || { echo "unsupported lock schema" >&2; exit 1; }
[[ "$gstreamer_version" =~ ^1\.28\.[0-9]+$ ]] || { echo "unsupported GStreamer version" >&2; exit 1; }
[[ "$gstreamer_sha" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid GStreamer checksum" >&2; exit 1; }
[[ "$libav_sha" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid gst-libav checksum" >&2; exit 1; }
[[ "$zlib_version" == 1.3.1 ]] || { echo "unsupported zlib version" >&2; exit 1; }
[[ "$zlib_filename" == zlib-1.3.1.tar.gz ]] || { echo "unsupported zlib filename" >&2; exit 1; }
[[ "$zlib_url" == https://gstreamer.freedesktop.org/src/mirror/zlib/zlib-1.3.1.tar.gz ]] || {
    echo "unsupported zlib source URL" >&2
    exit 1
}
[[ "$zlib_sha" == 9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23 ]] || {
    echo "unsupported zlib checksum" >&2
    exit 1
}
[[ "$cerbero_archive_sha" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid Cerbero archive checksum" >&2; exit 1; }
[[ "$cerbero_tag_object" =~ ^[[:xdigit:]]{40}$ ]] || { echo "invalid Cerbero tag object" >&2; exit 1; }
[[ "$cerbero_commit" =~ ^[[:xdigit:]]{40}$ ]] || { echo "invalid Cerbero commit" >&2; exit 1; }
[[ "$cerbero_tag" == "$gstreamer_version" ]] || { echo "Cerbero/GStreamer tags differ" >&2; exit 1; }
[[ "$builder_image" =~ ^ubuntu@sha256:[[:xdigit:]]{64}$ ]] || { echo "builder image is not digest pinned" >&2; exit 1; }
[[ "$builder_platform" == linux/amd64 ]] || { echo "unsupported builder platform" >&2; exit 1; }
[[ "$target_os" == linux && "$target_architecture" == x86_64 ]] || {
    echo "unsupported runtime target" >&2
    exit 1
}
[[ "$target_distribution" == ubuntu && "$target_distribution_version" == 24.04 ]] || {
    echo "unsupported builder distribution" >&2
    exit 1
}
[[ "$target_glibc" == 2.39 ]] || { echo "unsupported glibc floor" >&2; exit 1; }
[[ "$artifact_type" == tarball && "$artifact_compression" == xz && "$artifact_split" == false ]] || {
    echo "unsupported Cerbero artifact format" >&2
    exit 1
}
[[ "${packages[*]}" == "gstreamer-1.0 gstreamer-1.0-libav" ]] || {
    echo "package set is not the approved #18 pair" >&2
    exit 1
}
[[ "${variants[*]}" == norust ]] || { echo "variants are not the approved minimal set" >&2; exit 1; }

if [[ -e "$output_dir" ]] && [[ -n "$(find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
    echo "output directory is not empty: $output_dir" >&2
    exit 1
fi
mkdir -p "$output_dir"

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-gstreamer-build.XXXXXX")
cleanup() {
    rm -rf "$work_dir"
}
trap cleanup EXIT

export HOME="$work_dir/home"
export XDG_CACHE_HOME="$work_dir/cache"
mkdir -p "$HOME" "$XDG_CACHE_HOME"

download_and_verify() {
    local url=$1
    local sha=$2
    local destination=$3
    mkdir -p "$(dirname -- "$destination")"
    curl -fL --retry 3 --retry-connrefused --max-filesize 100000000 "$url" -o "$destination"
    printf '%s  %s\n' "$sha" "$destination" | sha256sum -c -
}

cerbero_archive="$work_dir/cerbero.tar.gz"
download_and_verify "$cerbero_archive_url" "$cerbero_archive_sha" "$cerbero_archive"
cerbero_list="$work_dir/cerbero.list"
tar -tzf "$cerbero_archive" >"$cerbero_list"
cerbero_root=$(sed -n '1s|/.*||p' "$cerbero_list")
[[ -n "$cerbero_root" ]] || { echo "Cerbero archive has no root directory" >&2; exit 1; }
tar -xzf "$cerbero_archive" -C "$work_dir"
cerbero_dir="$work_dir/$cerbero_root"
[[ -x "$cerbero_dir/cerbero-uninstalled" ]] || { echo "Cerbero entrypoint missing" >&2; exit 1; }

grep -F "tarball_checksum = '$gstreamer_sha'" "$cerbero_dir/recipes/gstreamer-1.0.recipe" >/dev/null
grep -F "tarball_checksum = '$libav_sha'" "$cerbero_dir/recipes/gst-libav-1.0.recipe" >/dev/null
zlib_recipe_version=$(sed -n "s/^[[:space:]]*version = '\([^']*\)'$/\1/p" "$cerbero_dir/recipes/zlib.recipe")
zlib_recipe_sha=$(sed -n "s/^[[:space:]]*tarball_checksum = '\([^']*\)'$/\1/p" "$cerbero_dir/recipes/zlib.recipe")
[[ "$zlib_recipe_version" == "$zlib_version" ]] || {
    echo "pinned Cerbero zlib recipe version disagrees with lock" >&2
    exit 1
}
[[ "$zlib_recipe_sha" == "$zlib_sha" ]] || {
    echo "pinned Cerbero zlib recipe checksum disagrees with lock" >&2
    exit 1
}

# Seed Cerbero's source cache with the two lock-owned release tarballs. The
# remaining closure is fetched by Cerbero's pinned recipes in the fetch phase.
download_and_verify "$gstreamer_url" "$gstreamer_sha" \
    "$XDG_CACHE_HOME/cerbero-sources/gstreamer-1.0/gstreamer-${gstreamer_version}.tar.xz"
download_and_verify "$libav_url" "$libav_sha" \
    "$XDG_CACHE_HOME/cerbero-sources/$libav_package/$libav_filename"
download_and_verify "$zlib_url" "$zlib_sha" \
    "$XDG_CACHE_HOME/cerbero-sources/zlib-1.3.1/zlib-1.3.1.tar.gz"

cerbero=("$cerbero_dir/cerbero-uninstalled" --non-interactive -c "$cerbero_dir/config/linux.config" -v norust)
"${cerbero[@]}" fetch-bootstrap --system=no --toolchains=no --build-tools=yes --jobs=2
for package in "${packages[@]}"; do
    "${cerbero[@]}" fetch-package "$package" --deps --jobs=2
done
"${cerbero[@]}" bootstrap --system=no --toolchains=no --build-tools=yes --offline --assume-yes --jobs=2

package_dir="$work_dir/packages"
mkdir -p "$package_dir"
for package in "${packages[@]}"; do
    mkdir -p "$package_dir/$package"
    "${cerbero[@]}" package "$package" \
        --artifact=tarball --compress-method=xz --no-split --no-devel --offline --jobs=2 \
        --output-dir "$package_dir/$package"
done

bundle="$work_dir/bundle"
runtime_root="$bundle/vendor/linux-x86_64"
mkdir -p "$runtime_root"

validate_package_archive() {
    local archive=$1
    local member_list=$2
    local member

    if ! tar -tJf "$archive" --quoting-style=escape >"$member_list"; then
        fail "cannot list package archive: $archive"
    fi
    [[ -s "$member_list" ]] || fail "package archive is empty: $archive"
    while IFS= read -r member; do
        case "$member" in
            /*|./*|*/./*|*/.|..|../*|*/../*|*/..|*//* )
                fail "unsafe package member in $archive: $member"
                ;;
        esac
        case "$member" in
            opt|opt/|opt/gstreamer-1.0|opt/gstreamer-1.0/|opt/gstreamer-1.0/*)
                ;;
            *)
                fail "package member outside opt/gstreamer-1.0 in $archive: $member"
                ;;
        esac
    done <"$member_list"
}

assert_staging_prefix() {
    local staging=$1
    local prefix="$staging/opt/gstreamer-1.0"
    local list_stem="$work_dir/${staging##*/}"
    local roots_list="$list_stem.roots"
    local opt_children_list="$list_stem.opt-children"
    local -a roots=()
    local -a opt_children=()

    if ! find -P "$staging" -mindepth 1 -maxdepth 1 -printf '%f\0' >"$roots_list"; then
        fail "cannot enumerate package staging roots: $staging"
    fi
    if ! mapfile -d '' -t roots <"$roots_list"; then
        fail "cannot read package staging roots: $roots_list"
    fi
    [[ ${#roots[@]} -eq 1 && "${roots[0]}" == opt ]] || {
        fail "package staging has roots outside opt: $staging"
    }
    [[ -d "$staging/opt" && ! -L "$staging/opt" ]] || {
        fail "package staging opt prefix is not a directory: $staging/opt"
    }

    if ! find -P "$staging/opt" -mindepth 1 -maxdepth 1 -printf '%f\0' >"$opt_children_list"; then
        fail "cannot enumerate opt package staging roots: $staging/opt"
    fi
    if ! mapfile -d '' -t opt_children <"$opt_children_list"; then
        fail "cannot read opt package staging roots: $opt_children_list"
    fi
    [[ ${#opt_children[@]} -eq 1 && "${opt_children[0]}" == gstreamer-1.0 ]] || {
        fail "package staging has roots outside opt/gstreamer-1.0: $staging"
    }
    [[ -d "$prefix" && ! -L "$prefix" ]] || {
        fail "package staging prefix is not a directory: $prefix"
    }
}

merge_package_prefix() {
    local prefix=$1
    local source_list=$2
    local prefix_root prefix_mode source source_parent rel destination source_kind source_target destination_target resolved_target

    capture_path prefix_root "package prefix path" realpath -m -- "$prefix"
    if ! prefix_mode=$(stat -c '%a' -- "$prefix"); then
        fail "cannot read package prefix mode: $prefix"
    fi
    if [[ -z "$expected_prefix_mode" ]]; then
        expected_prefix_mode=$prefix_mode
    elif [[ "$expected_prefix_mode" != "$prefix_mode" ]]; then
        fail "package prefix mode differs: $prefix ($prefix_mode), expected $expected_prefix_mode"
    fi

    if ! find -P "$prefix" -mindepth 1 -print0 >"$source_list"; then
        fail "cannot enumerate package prefix: $prefix"
    fi

    while IFS= read -r -d '' source; do
        rel=${source#"$prefix"/}
        destination="$runtime_root/$rel"

        if [[ -L "$source" ]]; then
            source_kind=symlink
            capture_path source_target "package symlink target at $source" readlink -- "$source"
            [[ "$source_target" != /* ]] || {
                fail "absolute package symlink target at $source: $source_target"
            }
            source_parent=${source%/*}
            capture_path resolved_target "package symlink resolution at $source" realpath -m -- "$source_parent/$source_target"
            case "$resolved_target/" in
                "$prefix_root/"*)
                    ;;
                *)
                    fail "package symlink escapes prefix at $source: $source_target -> $resolved_target"
                    ;;
            esac
        elif [[ -d "$source" ]]; then
            source_kind=directory
        elif [[ -f "$source" ]]; then
            source_kind=file
        else
            fail "special file in package prefix: $source"
        fi

        if [[ ! -L "$destination" && ! -e "$destination" ]]; then
            continue
        fi

        case "$source_kind" in
            symlink)
                [[ -L "$destination" ]] || {
                    fail "package collision changes type at $destination"
                }
                capture_path destination_target "destination symlink target at $destination" readlink -- "$destination"
                [[ "$source_target" == "$destination_target" ]] || {
                    fail "package collision changes symlink target at $destination"
                }
                ;;
            directory)
                [[ ! -L "$destination" && -d "$destination" ]] || {
                    fail "package collision changes type at $destination"
                }
                [[ "$(stat -c '%a' -- "$source")" == "$(stat -c '%a' -- "$destination")" ]] || {
                    fail "package collision changes directory mode at $destination"
                }
                ;;
            file)
                [[ ! -L "$destination" && -f "$destination" ]] || {
                    fail "package collision changes type at $destination"
                }
                cmp -s -- "$source" "$destination" || {
                    fail "package collision changes file contents at $destination"
                }
                [[ "$(stat -c '%a' -- "$source")" == "$(stat -c '%a' -- "$destination")" ]] || {
                    fail "package collision changes file mode at $destination"
                }
                ;;
        esac
    done <"$source_list"

    cp -a -- "$prefix"/. "$runtime_root"/
}

package_count=0
expected_prefix_mode=
package_list="$work_dir/package-tarballs.list"
if ! find -P "$package_dir" -type f -name '*.tar.xz' -print0 | sort -z >"$package_list"; then
    fail "cannot discover Cerbero package tarballs"
fi
while IFS= read -r -d '' package_tarball; do
    package_count=$((package_count + 1))
    staging="$work_dir/package-staging-$package_count"
    member_list="$work_dir/package-$package_count.members"
    source_list="$work_dir/package-$package_count.sources"
    mkdir -p "$staging"
    validate_package_archive "$package_tarball" "$member_list"
    tar -xJf "$package_tarball" -C "$staging" --no-same-owner
    assert_staging_prefix "$staging"
    merge_package_prefix "$staging/opt/gstreamer-1.0" "$source_list"
done <"$package_list"
[[ "$package_count" == 2 ]] || { echo "expected two Cerbero package tarballs, got $package_count" >&2; exit 1; }
[[ -n "$expected_prefix_mode" ]] || fail "package prefix mode was not established"
if ! chmod "$expected_prefix_mode" "$runtime_root"; then
    fail "cannot set runtime root mode: $runtime_root"
fi

launcher="$runtime_root/bin/lumina-gstreamer-runtime"
mkdir -p "$(dirname -- "$launcher")"
cat >"$launcher" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo "lumina-gstreamer-runtime: $*" >&2
    exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
runtime_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
lib_dir="$runtime_root/lib"
plugin_dir="$lib_dir/gstreamer-1.0"
scanner="$runtime_root/libexec/gstreamer-1.0/gst-plugin-scanner"

[[ -d "$lib_dir" ]] || fail "private library directory is missing: $lib_dir"
[[ -d "$plugin_dir" ]] || fail "private plugin directory is missing: $plugin_dir"
[[ -x "$scanner" ]] || fail "private plugin scanner is missing: $scanner"

cache_root=${XDG_CACHE_HOME:-}
if [[ -z "$cache_root" ]]; then
    [[ -n "${HOME:-}" ]] || fail 'HOME and XDG_CACHE_HOME are unset'
    cache_root="$HOME/.cache"
fi
[[ "$cache_root" == /* ]] || fail "cache root must be absolute: $cache_root"
mkdir -p "$cache_root" || fail "cannot create cache root: $cache_root"
cache_root=$(CDPATH= cd -- "$cache_root" && pwd -P) || fail "cannot resolve cache root"
case "$cache_root/" in
    "$runtime_root/"*) fail 'cache must be outside the runtime bundle' ;;
esac

cache_dir="$cache_root/lumina-video/gstreamer-1.0"
mkdir -p "$cache_dir" || fail "cannot create private cache: $cache_dir"
cache_dir=$(CDPATH= cd -- "$cache_dir" && pwd -P) || fail "cannot resolve private cache"
case "$cache_dir/" in
    "$cache_root/"*) ;;
    *) fail 'private cache escaped XDG_CACHE_HOME' ;;
esac
case "$cache_dir/" in
    "$runtime_root/"*) fail 'private cache is inside the runtime bundle' ;;
esac
[[ -d "$cache_dir" && -w "$cache_dir" ]] || fail "private cache is not writable: $cache_dir"
registry_path="$cache_dir/gstreamer-1.0.registry"
if [[ -L "$registry_path" ]]; then
    fail "registry path must not be a symlink: $registry_path"
fi
if [[ -e "$registry_path" && ! -f "$registry_path" ]]; then
    fail "registry path is not a regular file: $registry_path"
fi

export XDG_CACHE_HOME="$cache_root"
export LD_LIBRARY_PATH="$lib_dir"
export GST_PLUGIN_PATH_1_0="$plugin_dir"
export GST_PLUGIN_SYSTEM_PATH_1_0=
export GST_PLUGIN_PATH=
export GST_PLUGIN_SYSTEM_PATH=
export GST_PLUGIN_SCANNER_1_0="$scanner"
export GST_PLUGIN_SCANNER=
export GST_REGISTRY_1_0="$registry_path"
export GST_REGISTRY=
export GST_REGISTRY_REUSE_PLUGIN_SCANNER=no

(($#)) || fail 'usage: lumina-gstreamer-runtime COMMAND [ARGUMENT ...]'
exec "$@"
EOF
chmod 0755 "$launcher"

plugin_dir="$runtime_root/lib/gstreamer-1.0"
scanner="$runtime_root/libexec/gstreamer-1.0/gst-plugin-scanner"
[[ -d "$plugin_dir" ]] || { echo "plugin directory missing from package union" >&2; exit 1; }
[[ -x "$scanner" ]] || { echo "plugin scanner missing from package union" >&2; exit 1; }
[[ -x "$launcher" ]] || { echo "runtime launcher missing from package union" >&2; exit 1; }

jq -r '.required_elements[] | .filename' "$lock_file" | while IFS= read -r filename; do
    [[ -f "$plugin_dir/$filename" ]] || {
        echo "required plugin file missing: $filename" >&2
        exit 1
    }
done

cat >"$runtime_root/VERSION" <<EOF
GSTREAMER_VERSION=$gstreamer_version
CERBERO_TAG=$cerbero_tag
CERBERO_COMMIT=$cerbero_commit
TARGET=linux-x86_64
GLIBC_FLOOR=$target_glibc
EOF
cat >"$bundle/NOTICE" <<'EOF'
This standalone runtime is the union of upstream Cerbero packages named in
vendor/gstreamer-1.0.lock.json. It is an upstream meta-package bootstrap for
Lumina, not the recursive closure/license/source inventory planned for #19.
EOF

# Normalize the generated vendor tree before hashing so the tree digest covers
# the exact files that will be packed.
find "$runtime_root" -exec touch -h -d '@0' {} +
(cd "$bundle" && find vendor -type f -print0 | sort -z |
    while IFS= read -r -d '' file; do sha256sum "$file"; done) >"$bundle/tree.sha256"
tree_sha=$(sha256sum "$bundle/tree.sha256" | awk '{ print $1 }')
jq -n \
    --arg version "$gstreamer_version" \
    --arg commit "$cerbero_commit" \
    --arg tree_sha "$tree_sha" \
    '{gstreamer_version: $version, cerbero_commit: $commit, tree_sha256: $tree_sha, packages: ["gstreamer-1.0", "gstreamer-1.0-libav"]}' \
    >"$bundle/runtime-manifest.json"

# Normalize metadata after writing the manifest; vendor contents and tree.sha256
# are unchanged, so tree_sha still describes the final vendor tree.
find "$bundle" -maxdepth 1 -exec touch -h -d '@0' {} +

artifact_name=gstreamer-runtime-linux-x86_64.tar.gz
tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
    -czf "$output_dir/$artifact_name" -C "$bundle" .
(cd "$output_dir" && sha256sum "$artifact_name" >"$artifact_name.sha256")
printf '%s  tree\n' "$tree_sha" >"$output_dir/$artifact_name.tree.sha256"
echo "created $output_dir/$artifact_name"
