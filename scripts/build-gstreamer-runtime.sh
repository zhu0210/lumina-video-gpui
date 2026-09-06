#!/usr/bin/env bash
set -euo pipefail

# Formal build: every version, URL, checksum, package, and variant comes from
# the lock. This script never discovers moving upstream metadata.

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
lock_file="$repo_root/vendor/gstreamer-1.0.lock.json"
output_dir="$repo_root/dist/gstreamer-runtime"
build_demo=false
build_cargo_home=${CARGO_HOME:-$HOME/.cargo}
build_rustup_home=${RUSTUP_HOME:-$HOME/.rustup}

usage() {
    echo "Usage: $0 [--lock PATH] [--output DIR] [--build-demo]" >&2
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
        --build-demo)
            build_demo=true
            shift
            ;;
        *) usage ;;
    esac
done

for command_name in curl jq sha256sum tar xz gzip find sort awk grep sed mktemp realpath chmod cmp cp readlink stat readelf; do
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

validate_symlink_target() {
    local source=$1
    local boundary_path=$2
    local description=$3
    local result_var=$4
    local target source_parent resolved_target

    capture_path target "$description target at $source" readlink -- "$source"
    [[ "$target" != /* ]] || {
        fail "absolute $description target at $source: $target"
    }
    source_parent=${source%/*}
    capture_path resolved_target "$description resolution at $source" \
        realpath -m -- "$source_parent/$target"
    case "$resolved_target/" in
        "$boundary_path/"*)
            ;;
        *)
            fail "$description escapes boundary at $source: $target -> $resolved_target"
            ;;
    esac
    printf -v "$result_var" '%s' "$target"
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
webrtc_version=$(jq -er '.sources.webrtc_audio_processing.version' "$lock_file")
webrtc_url=$(jq -er '.sources.webrtc_audio_processing.url' "$lock_file")
webrtc_sha=$(jq -er '.sources.webrtc_audio_processing.sha256' "$lock_file")
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
archive_source_libdir=$(jq -er '.artifact.archive_layout.source_libdir' "$lock_file")
archive_runtime_libdir=$(jq -er '.artifact.archive_layout.runtime_libdir' "$lock_file")
mapfile -t packages < <(jq -er '.packages[]' "$lock_file")
mapfile -t variants < <(jq -er '.variants[]' "$lock_file")

[[ "$schema_version" == 1 ]] || { echo "unsupported lock schema" >&2; exit 1; }
[[ "$gstreamer_version" =~ ^1\.28\.[0-9]+$ ]] || { echo "unsupported GStreamer version" >&2; exit 1; }
[[ "$gstreamer_sha" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid GStreamer checksum" >&2; exit 1; }
[[ "$libav_sha" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid gst-libav checksum" >&2; exit 1; }
[[ "$webrtc_version" =~ ^[0-9]+\.[0-9]+$ ]] || fail "invalid WebRTC audio processing version"
[[ "$webrtc_sha" =~ ^[[:xdigit:]]{64}$ ]] || fail "invalid WebRTC audio processing checksum"
[[ "$webrtc_url" == "https://www.freedesktop.org/software/pulseaudio/webrtc-audio-processing/webrtc-audio-processing-${webrtc_version}.tar.gz" ]] || fail "unexpected WebRTC audio processing source"
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
[[ "$archive_source_libdir" == lib/x86_64-linux-gnu ]] || {
    echo "unsupported Cerbero source library directory" >&2
    exit 1
}
[[ "$archive_runtime_libdir" == lib/x86_64-linux-gnu ]] || {
    echo "unsupported Cerbero runtime library directory" >&2
    exit 1
}
[[ "$archive_source_libdir" == "$archive_runtime_libdir" ]] || {
    echo "source and runtime library directories differ" >&2
    exit 1
}
[[ "${packages[*]}" == "gstreamer-1.0 gstreamer-1.0-libav" ]] || {
    echo "package set is not the approved #18 pair" >&2
    exit 1
}
[[ "${variants[*]}" == norust ]] || { echo "variants are not the approved minimal set" >&2; exit 1; }

layout_keys=$(jq -er '.artifact.archive_layout.package_roots | keys[]' "$lock_file" | sort)
package_keys=$(printf '%s\n' "${packages[@]}" | sort)
[[ "$layout_keys" == "$package_keys" ]] || {
    echo "archive layout package roots do not match the package set" >&2
    exit 1
}

for package in "${packages[@]}"; do
    package_roots_text=$(jq -er --arg package "$package" \
        '.artifact.archive_layout.package_roots[$package][]' "$lock_file")
    mapfile -t package_roots <<<"$package_roots_text"
    case "$package" in
        gstreamer-1.0)
            [[ "${package_roots[*]}" == "bin etc lib libexec share" ]] || {
                echo "unexpected archive layout roots for $package" >&2
                exit 1
            }
            ;;
        gstreamer-1.0-libav)
            [[ "${package_roots[*]}" == lib ]] || {
                echo "unexpected archive layout roots for $package" >&2
                exit 1
            }
            ;;
    esac
done

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

build_home="$work_dir/home"
build_cache_home="$work_dir/cache"
mkdir -p "$build_home" "$build_cache_home"

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

# Cerbero 1.28's FFmpeg recipe carries a 7.1-only Meson port. Replace build
# metadata with our configure-based recipe; never apply those patches to 9.x.
python3 - "$lock_file" "$repo_root/vendor/ffmpeg.lock.json" "$script_dir/cerbero-ffmpeg.recipe" "$cerbero_dir/recipes/ffmpeg.recipe" <<'PY_FFMPEG'
import json, pathlib, re, sys
runtime, shared, template, target = map(pathlib.Path, sys.argv[1:])
locked = json.loads(runtime.read_text())["sources"]["ffmpeg"]
assert locked == json.loads(shared.read_text()), "FFmpeg runtime and macOS locks differ"
assert re.fullmatch(r"9\.\d+\.\d+", locked["version"])
assert re.fullmatch(r"[0-9a-f]{64}", locked["sha256"])
assert locked["url"] == f'https://ffmpeg.org/releases/ffmpeg-{locked["version"]}.tar.xz'
target.write_text(template.read_text().replace("@VERSION@", locked["version"]).replace("@SHA256@", locked["sha256"]))
PY_FFMPEG

grep -F "tarball_checksum = '$gstreamer_sha'" "$cerbero_dir/recipes/gstreamer-1.0.recipe" >/dev/null
grep -F "tarball_checksum = '$libav_sha'" "$cerbero_dir/recipes/gst-libav-1.0.recipe" >/dev/null
grep -F "version = '$webrtc_version'" "$cerbero_dir/recipes/webrtc-audio-processing.recipe" >/dev/null
grep -F "tarball_checksum = '$webrtc_sha'" "$cerbero_dir/recipes/webrtc-audio-processing.recipe" >/dev/null
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

# Seed Cerbero's source cache with the lock-owned release tarballs. The
# remaining closure is fetched by Cerbero's pinned recipes in the fetch phase.
download_and_verify "$gstreamer_url" "$gstreamer_sha" \
    "$build_cache_home/cerbero-sources/gstreamer-1.0/gstreamer-${gstreamer_version}.tar.xz"
download_and_verify "$libav_url" "$libav_sha" \
    "$build_cache_home/cerbero-sources/$libav_package/$libav_filename"
download_and_verify "$zlib_url" "$zlib_sha" \
    "$build_cache_home/cerbero-sources/zlib-1.3.1/zlib-1.3.1.tar.gz"
# The bare freedesktop.org host rejects CI downloads (HTTP 418), while the
# canonical www host serves the identical archive. Preserve Cerbero's checksum.
download_and_verify "$webrtc_url" "$webrtc_sha" \
    "$build_cache_home/cerbero-sources/webrtc-audio-processing-${webrtc_version}/webrtc-audio-processing-${webrtc_version}.tar.gz"

# The WavPack origin has served different bytes for the same release name.
# Use the official mirror, still checked against the locked Cerbero recipe.
wavpack_version=$(sed -n "s/^[[:space:]]*version = '\([^']*\)'$/\1/p" "$cerbero_dir/recipes/wavpack.recipe")
wavpack_sha=$(sed -n "s/^[[:space:]]*tarball_checksum = '\([^']*\)'$/\1/p" "$cerbero_dir/recipes/wavpack.recipe")
[[ "$wavpack_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && "$wavpack_sha" =~ ^[[:xdigit:]]{64}$ ]] || fail "invalid locked WavPack recipe"
download_and_verify "https://gstreamer.freedesktop.org/src/mirror/wavpack/wavpack-${wavpack_version}.tar.xz" \
    "$wavpack_sha" "$build_cache_home/cerbero-sources/wavpack-${wavpack_version}/wavpack-${wavpack_version}.tar.xz"

cerbero=(env HOME="$build_home" XDG_CACHE_HOME="$build_cache_home" "$cerbero_dir/cerbero-uninstalled" --non-interactive -c "$cerbero_dir/config/linux.config" -v norust)
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
    local package=$3
    local top_root allowed
    local -a allowed_roots=()
    local member
    shift 3
    allowed_roots=("$@")

    if ! tar -tJf "$archive" --quoting-style=escape >"$member_list"; then
        fail "cannot list package archive: $archive"
    fi
    [[ -s "$member_list" ]] || fail "package archive is empty: $archive"
    while IFS= read -r member; do
        case "$member" in
            /*|./*|*/./*|*/.|.|..|../*|*/../*|*/..|*//* )
                fail "unsafe package member in $archive: $member"
                ;;
        esac
        if [[ "$member" == */* ]]; then
            top_root=${member%%/*}
        else
            top_root=$member
        fi
        allowed=0
        for allowed_root in "${allowed_roots[@]}"; do
            if [[ "$top_root" == "$allowed_root" ]]; then
                allowed=1
                break
            fi
        done
        [[ "$allowed" == 1 ]] || {
            fail "package member root is not allowed for $package: $member"
        }
    done <"$member_list"
}

assert_staging_roots() {
    local staging=$1
    local roots_list=$2
    local root allowed
    local -a roots=()
    local -a allowed_roots=()
    shift 2
    allowed_roots=("$@")

    if ! find -P "$staging" -mindepth 1 -maxdepth 1 -printf '%f\0' >"$roots_list"; then
        fail "cannot enumerate package staging roots: $staging"
    fi
    if ! mapfile -d '' -t roots <"$roots_list"; then
        fail "cannot read package staging roots: $roots_list"
    fi
    [[ ${#roots[@]} -gt 0 ]] || fail "package staging has no top-level roots: $staging"
    for root in "${roots[@]}"; do
        allowed=0
        for allowed_root in "${allowed_roots[@]}"; do
            if [[ "$root" == "$allowed_root" ]]; then
                allowed=1
                break
            fi
        done
        [[ "$allowed" == 1 ]] || {
            fail "package staging has an unapproved top-level root: $staging/$root"
        }
    done
}

merge_tree() {
    local source_root=$1
    local destination_root=$2
    local source_list=$3
    local source_boundary=$4
    local source_boundary_path root_mode source_mode destination_mode source rel destination source_kind source_target destination_target

    (($# == 4)) || fail "package merge requires a source boundary"
    [[ -d "$source_boundary" && ! -L "$source_boundary" ]] || {
        fail "package merge source boundary is not a real directory: $source_boundary"
    }
    capture_path source_boundary_path "package merge source boundary" realpath -m -- "$source_boundary"
    [[ -d "$source_boundary_path" && ! -L "$source_boundary_path" ]] || {
        fail "package merge canonical source boundary is not a real directory: $source_boundary_path"
    }

    [[ -d "$source_root" && ! -L "$source_root" ]] || {
        fail "package merge source root is not a real directory: $source_root"
    }
    if ! root_mode=$(stat -c '%a' -- "$source_root"); then
        fail "cannot read package merge source root mode: $source_root"
    fi

    if [[ -L "$destination_root" ]]; then
        fail "package merge destination root is a symlink: $destination_root"
    elif [[ -e "$destination_root" ]]; then
        [[ -d "$destination_root" ]] || fail "package merge destination root is not a directory: $destination_root"
        if ! destination_mode=$(stat -c '%a' -- "$destination_root"); then
            fail "cannot read package merge destination root mode: $destination_root"
        fi
        [[ "$root_mode" == "$destination_mode" ]] || {
            fail "package merge root mode differs at $destination_root"
        }
    else
        if ! mkdir -p -- "$destination_root"; then
            fail "cannot create package merge destination root: $destination_root"
        fi
        if ! chmod "$root_mode" "$destination_root"; then
            fail "cannot set package merge destination root mode: $destination_root"
        fi
    fi

    if ! find -P "$source_root" -mindepth 1 -print0 >"$source_list"; then
        fail "cannot enumerate package merge source root: $source_root"
    fi

    while IFS= read -r -d '' source; do
        rel=${source#"$source_root"/}
        destination="$destination_root/$rel"

        if [[ -L "$source" ]]; then
            source_kind=symlink
            validate_symlink_target "$source" "$source_boundary_path" \
                "package symlink" source_target
        elif [[ -d "$source" ]]; then
            source_kind=directory
        elif [[ -f "$source" ]]; then
            source_kind=file
        else
            fail "special file in package merge source: $source"
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
                if ! source_mode=$(stat -c '%a' -- "$source"); then
                    fail "cannot read source directory mode: $source"
                fi
                if ! destination_mode=$(stat -c '%a' -- "$destination"); then
                    fail "cannot read destination directory mode: $destination"
                fi
                [[ "$source_mode" == "$destination_mode" ]] || {
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
                if ! source_mode=$(stat -c '%a' -- "$source"); then
                    fail "cannot read source file mode: $source"
                fi
                if ! destination_mode=$(stat -c '%a' -- "$destination"); then
                    fail "cannot read destination file mode: $destination"
                fi
                [[ "$source_mode" == "$destination_mode" ]] || {
                    fail "package collision changes file mode at $destination"
                }
                ;;
        esac
    done <"$source_list"

    cp -a -- "$source_root"/. "$destination_root"/
    if ! chmod "$root_mode" "$destination_root"; then
        fail "cannot finalize package merge destination root mode: $destination_root"
    fi
}

assert_symlink_tree() {
    local root=$1
    local root_path symlink_list source source_target

    [[ -d "$root" && ! -L "$root" ]] || {
        fail "runtime symlink root is not a real directory: $root"
    }
    capture_path root_path "runtime symlink root" realpath -m -- "$root"
    [[ -d "$root_path" && ! -L "$root_path" ]] || {
        fail "canonical runtime symlink root is not a real directory: $root_path"
    }
    symlink_list="$work_dir/runtime-symlinks"
    if ! find -P "$root" -type l -print0 >"$symlink_list"; then
        fail "cannot enumerate runtime symlinks: $root"
    fi
    while IFS= read -r -d '' source; do
        validate_symlink_target "$source" "$root_path" "runtime symlink" source_target
    done <"$symlink_list"
}

package_count=0
if ! chmod 0755 "$runtime_root"; then
    fail "cannot set runtime root mode: $runtime_root"
fi
for package in "${packages[@]}"; do
    package_count=$((package_count + 1))
    package_path="$package_dir/$package"
    [[ -d "$package_path" && ! -L "$package_path" ]] || {
        fail "package output directory is missing or not a real directory: $package_path"
    }
    package_tarballs_list="$work_dir/package-$package_count.tarballs"
    if ! find -P "$package_path" -type f -name '*.tar.xz' -print0 >"$package_tarballs_list"; then
        fail "cannot discover package tarball: $package_path"
    fi
    package_tarballs=()
    if ! mapfile -d '' -t package_tarballs <"$package_tarballs_list"; then
        fail "cannot read package tarball list: $package_tarballs_list"
    fi
    [[ ${#package_tarballs[@]} -eq 1 ]] || {
        fail "expected exactly one package tarball for $package, got ${#package_tarballs[@]}"
    }
    package_tarball=${package_tarballs[0]}
    [[ -f "$package_tarball" && ! -L "$package_tarball" ]] || {
        fail "package tarball is not a regular file: $package_tarball"
    }

    package_roots_text=$(jq -er --arg package "$package" \
        '.artifact.archive_layout.package_roots[$package][]' "$lock_file")
    mapfile -t package_roots <<<"$package_roots_text"
    staging="$work_dir/package-staging-$package_count"
    member_list="$work_dir/package-$package_count.members"
    roots_list="$work_dir/package-$package_count.roots"
    if [[ -L "$staging" || -e "$staging" ]]; then
        fail "package staging paths are not fresh: $package"
    fi
    validate_package_archive "$package_tarball" "$member_list" "$package" "${package_roots[@]}"
    mkdir "$staging"
    tar -xJf "$package_tarball" -C "$staging" --no-same-owner
    assert_staging_roots "$staging" "$roots_list" "${package_roots[@]}"
    [[ -d "$staging/$archive_source_libdir" && ! -L "$staging/$archive_source_libdir" ]] || {
        fail "package staging source library directory is not a real directory: $staging/$archive_source_libdir"
    }
    [[ -d "$staging/$archive_runtime_libdir" && ! -L "$staging/$archive_runtime_libdir" ]] || {
        fail "package staging runtime library directory is not a real directory: $staging/$archive_runtime_libdir"
    }
    actual_roots=()
    if ! mapfile -d '' -t actual_roots <"$roots_list"; then
        fail "cannot read package staging roots: $roots_list"
    fi
    for root in "${actual_roots[@]}"; do
        merge_tree "$staging/$root" "$runtime_root/$root" \
            "$work_dir/package-$package_count-$root.sources" "$staging"
    done
done
[[ "$package_count" == 2 ]] || fail "expected exactly two lock packages, got $package_count"
if ! chmod 0755 "$runtime_root"; then
    fail "cannot finalize runtime root mode: $runtime_root"
fi
assert_symlink_tree "$runtime_root"

launcher="$runtime_root/bin/lumina-gstreamer-runtime"
mkdir -p "$(dirname -- "$launcher")"
{
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
    printf 'runtime_libdir=%q\n' "$archive_runtime_libdir"
    cat <<'EOF'

fail() {
    echo "lumina-gstreamer-runtime: $*" >&2
    exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
runtime_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
lib_dir="$runtime_root/$runtime_libdir"
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
export GIO_MODULE_DIR="$lib_dir/gio/modules"
export GIO_EXTRA_MODULES=
export GIO_USE_TLS=openssl
# glib-networking's OpenSSL backend uses X509_STORE_set_default_paths, which
# honors these variables while retaining normal certificate validation.
export SSL_CERT_FILE="$runtime_root/etc/ssl/certs/ca-certificates.crt"
export SSL_CERT_DIR="$runtime_root/etc/ssl/certs"

(($#)) || fail 'usage: lumina-gstreamer-runtime COMMAND [ARGUMENT ...]'
exec "$@"
EOF
} >"$launcher"
chmod 0755 "$launcher"

plugin_dir="$runtime_root/$archive_runtime_libdir/gstreamer-1.0"
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
This standalone runtime contains the upstream Cerbero packages named in
vendor/gstreamer-1.0.lock.json and their ELF dependency closure from the same
Cerbero prefix and locked Ubuntu builder. elf-dependencies.json records added
libraries and host-owned glibc/driver boundaries. The full license/source
inventory remains tracked in #19.
EOF

if $build_demo; then
    build_cargo=$(command -v cargo) || fail "cargo is required for --build-demo"
    build_target_dir="$repo_root/target/bundled-linux"
    "${cerbero[@]}" run env CARGO_HOME="$build_cargo_home" RUSTUP_HOME="$build_rustup_home" \
        PKG_CONFIG_LIBDIR=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
        CARGO_TARGET_DIR="$build_target_dir" "$build_cargo" build \
        --manifest-path "$repo_root/Cargo.toml" --release --locked \
        --package lumina-video-demo --features vendored-runtime
    mkdir -p "$bundle/bin"
    cp "$build_target_dir/release/lumina-video-demo" "$bundle/bin/"
    cat >"$bundle/lumina-video-demo" <<'APP_LAUNCHER'
#!/usr/bin/env bash
set -euo pipefail
bundle_root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec "$bundle_root/vendor/linux-x86_64/bin/lumina-gstreamer-runtime" \
    "$bundle_root/bin/lumina-video-demo" "$@"
APP_LAUNCHER
    chmod 0755 "$bundle/lumina-video-demo"
fi

# Upstream Linux packages expect these dynamically loaded GIO plugins from
# the system. A private runtime must carry the modules from its own GLib build.
sdk_prefix="$cerbero_dir/build/dist/linux_x86_64"
sdk_libdir="$sdk_prefix/$archive_source_libdir"
mkdir -p "$runtime_root/$archive_runtime_libdir/gio/modules" "$runtime_root/etc/ssl/certs"
for module in libgioopenssl.so libgiolibproxy.so; do
    [[ -f "$sdk_libdir/gio/modules/$module" ]] || fail "GIO module missing from SDK: $module"
    cp -L "$sdk_libdir/gio/modules/$module" "$runtime_root/$archive_runtime_libdir/gio/modules/"
done
[[ -s /etc/ssl/certs/ca-certificates.crt ]] || fail "builder CA trust store missing"
cp /etc/ssl/certs/ca-certificates.crt "$runtime_root/etc/ssl/certs/"

# Exercise dlopen and GIO extension discovery in the clean smoke container;
# this does not require a display, Vulkan device, network, or media packages.
"${cerbero[@]}" run cc "$script_dir/runtime-load-probe.c" \
    -I"$sdk_prefix/include/glib-2.0" -I"$sdk_libdir/glib-2.0/include" \
    -L"$sdk_libdir" -lgio-2.0 -lgobject-2.0 -lglib-2.0 -ldl \
    -o "$runtime_root/bin/lumina-runtime-probe"

python3 "$script_dir/collect-runtime-libraries.py" \
    --bundle "$bundle" --prefix "$sdk_prefix" \
    --require-library libvulkan.so.1 \
    --libdir "$runtime_root/$archive_runtime_libdir" \
    --manifest "$runtime_root/elf-dependencies.json"
assert_symlink_tree "$runtime_root"

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
