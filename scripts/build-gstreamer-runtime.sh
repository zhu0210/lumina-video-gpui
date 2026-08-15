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

for command_name in curl jq sha256sum tar unzip xz find sort awk grep sed tr mktemp realpath chmod cmp cp readlink stat readelf patch python3; do
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
pipewire_version=$(jq -er '.sources.pipewire.version' "$lock_file")
pipewire_tag=$(jq -er '.sources.pipewire.tag' "$lock_file")
pipewire_tag_commit=$(jq -er '.sources.pipewire.tag_commit' "$lock_file")
pipewire_url=$(jq -er '.sources.pipewire.url' "$lock_file")
pipewire_sha=$(jq -er '.sources.pipewire.sha256' "$lock_file")
pipewire_license=$(jq -er '.sources.pipewire.license' "$lock_file")
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
mapfile -t recipe_allowlist < <(jq -er '.audit.recipe_allowlist[]' "$lock_file")
mapfile -t package_file_specs < <(jq -er '.audit.package_files[]' "$lock_file")
mapfile -t system_elf_allowlist < <(jq -er '.audit.system_elf_allowlist[]' "$lock_file")
mapfile -t forbidden_components < <(jq -er '.audit.policy.forbidden_components[]' "$lock_file")
mapfile -t license_texts < <(jq -er '.license_texts[]' "$lock_file")
flatpak_runtime=$(jq -er '.flatpak.runtime' "$lock_file")
flatpak_runtime_version=$(jq -er '.flatpak.runtime_version' "$lock_file")
flatpak_runtime_ref=$(jq -er '.flatpak.runtime_ref' "$lock_file")
flatpak_sdk=$(jq -er '.flatpak.sdk' "$lock_file")
flatpak_sdk_version=$(jq -er '.flatpak.sdk_version' "$lock_file")
flatpak_sdk_ref=$(jq -er '.flatpak.sdk_ref' "$lock_file")
flatpak_rust_extension=$(jq -er '.flatpak.rust_extension' "$lock_file")
flatpak_rust_ref=$(jq -er '.flatpak.rust_extension_ref' "$lock_file")
ffmpeg_version=$(jq -er '.components[] | select(.name == "FFmpeg") | .version' "$lock_file")
ffmpeg_url=$(jq -er '.components[] | select(.name == "FFmpeg") | .source_url' "$lock_file")
ffmpeg_sha=$(jq -er '.components[] | select(.name == "FFmpeg") | .sha256' "$lock_file")

[[ "$schema_version" == 2 ]] || { echo "unsupported lock schema" >&2; exit 1; }
[[ "$gstreamer_version" =~ ^1\.28\.[0-9]+$ ]] || { echo "unsupported GStreamer version" >&2; exit 1; }
[[ "$gstreamer_sha" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid GStreamer checksum" >&2; exit 1; }
[[ "$libav_sha" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid gst-libav checksum" >&2; exit 1; }
[[ "$zlib_version" == 1.3.1 ]] || { echo "unsupported zlib version" >&2; exit 1; }
[[ "$zlib_filename" == zlib-1.3.1.tar.gz ]] || { echo "unsupported zlib filename" >&2; exit 1; }
[[ "$zlib_url" == https://zlib.net/fossils/zlib-1.3.1.tar.gz ]] || {
    echo "unsupported zlib source URL" >&2
    exit 1
}
[[ "$zlib_sha" == 9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23 ]] || {
    echo "unsupported zlib checksum" >&2
    exit 1
}
[[ "$pipewire_version" == 1.6.8 && "$pipewire_tag" == 1.6.8 ]] || {
    echo "unsupported PipeWire version/tag" >&2
    exit 1
}
[[ "$pipewire_tag_commit" == b741e0c74f5436f0c925f7741140db0efd32cf4e ]] || {
    echo "unsupported PipeWire tag commit" >&2
    exit 1
}
[[ "$pipewire_url" == https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/b741e0c74f5436f0c925f7741140db0efd32cf4e/pipewire-b741e0c74f5436f0c925f7741140db0efd32cf4e.tar.gz ]] || {
    echo "unsupported PipeWire source URL" >&2
    exit 1
}
[[ "$pipewire_sha" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid PipeWire checksum" >&2; exit 1; }
[[ "$pipewire_license" == MIT ]] || { echo "unsupported PipeWire license metadata" >&2; exit 1; }
jq -e '
    .sources.pipewire.plugin_license == "MIT/X11" and
    .sources.pipewire.plugin_license_source == "src/gst" and
    any(.audit.plugin_allowlist[]; .element == "pipewiresink" and .filename == "libgstpipewire.so" and .owner_component == "PipeWire" and .source == "pipewire/src/gst" and .license == "MIT/X11")
' "$lock_file" >/dev/null || {
    echo "PipeWire GStreamer plugin source/license metadata is incomplete" >&2
    exit 1
}
[[ "$ffmpeg_version" == 7.1 ]] || { echo "unsupported FFmpeg version" >&2; exit 1; }
[[ "$ffmpeg_url" == https://ffmpeg.org/releases/ffmpeg-7.1.tar.xz ]] || { echo "unsupported FFmpeg source URL" >&2; exit 1; }
[[ "$ffmpeg_sha" == 40973d44970dbc83ef302b0609f2e74982be2d85916dd2ee7472d30678a7abe6 ]] || {
    echo "unsupported FFmpeg checksum" >&2
    exit 1
}
[[ "$(jq -er '.components[] | select(.name == "PipeWire") | .tag_commit' "$lock_file")" == "$pipewire_tag_commit" ]] || {
    echo "PipeWire component/tag metadata disagrees" >&2
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
[[ "${packages[*]}" == "lumina-audited" ]] || {
    echo "package set must contain only lumina-audited" >&2
    exit 1
}
[[ "${variants[*]}" == "norust alsa pulse va" ]] || { echo "variants are not the audited set" >&2; exit 1; }
[[ "${recipe_allowlist[0]}" == "gstreamer-1.0" ]] || {
    echo "recipe allowlist must start with gstreamer-1.0" >&2
    exit 1
}
[[ "${flatpak_runtime}:${flatpak_runtime_version}:${flatpak_sdk}:${flatpak_sdk_version}" == \
    "org.freedesktop.Platform:25.08:org.freedesktop.Sdk:25.08" ]] || {
    echo "Flatpak is not locked to Freedesktop 25.08" >&2
    exit 1
}
[[ "$flatpak_rust_extension" == org.freedesktop.Sdk.Extension.rust-stable ]] || {
    echo "Flatpak Rust extension is not the approved stable extension" >&2
    exit 1
}
[[ "$flatpak_runtime_ref:$flatpak_sdk_ref:$flatpak_rust_ref" == \
    "org.freedesktop.Platform//25.08:org.freedesktop.Sdk//25.08:org.freedesktop.Sdk.Extension.rust-stable//25.08" ]] || {
    echo "Flatpak refs are not the audited 25.08 refs" >&2
    exit 1
}
jq -e '
    any(.flatpak.source_metadata[]; .name == "freedesktop-platform" and .source_url == "https://github.com/flathub/org.freedesktop.Platform" and .license == "MIT") and
    any(.flatpak.source_metadata[]; .name == "freedesktop-sdk" and .source_url == "https://gitlab.com/freedesktop-sdk/freedesktop-sdk" and (.license | startswith("LGPL")))
' "$lock_file" >/dev/null || {
    echo "Flatpak source/license metadata is incomplete" >&2
    exit 1
}
for policy_key in gst_bad_gpl gst_bad_ugly gst_libav_ffmpeg_gpl \
    gst_libav_ffmpeg_nonfree gst_libav_ffmpeg_version3 gst_libav_ffmpeg_external_x264; do
    [[ "$(jq -er --arg key "$policy_key" '.audit.policy[$key]' "$lock_file")" == false ]] || {
        echo "audited policy must disable $policy_key" >&2
        exit 1
    }
done
[[ "$(jq -er '.audit.policy.software_fallback.video' "$lock_file")" == avdec_h264 ]] || {
    echo "H.264 fallback is not avdec_h264" >&2
    exit 1
}
[[ "$(jq -er '.audit.policy.software_fallback.audio' "$lock_file")" == avdec_aac ]] || {
    echo "AAC fallback is not avdec_aac" >&2
    exit 1
}
jq -e 'all(.components[]; (.name and .version and .source_url and (.sha256 | test("^[[:xdigit:]]{64}$")) and .license))' "$lock_file" >/dev/null || {
    echo "component inventory is incomplete" >&2
    exit 1
}
jq -e '([.components[].sha256] | length) == ([.components[].sha256] | unique | length)' "$lock_file" >/dev/null || {
    echo "component source SHA-256 values must be unique for ownership matching" >&2
    exit 1
}
jq -e 'all(.components[]; ((.license | startswith("LGPL")) or (.license == "Zlib") or (.license | startswith("MIT")) or (.license | startswith("BSD")) or (.license == "BZIP2-1.0.6") or (.license == "Apache-2.0") or (.license == "Public Domain")))' "$lock_file" >/dev/null || {
    echo "component license policy rejects an unapproved component" >&2
    exit 1
}
jq -e 'all(.audit.plugin_allowlist[]; (.filename and .element and .source and .owner_component and .license_source_url and (.license | test("^(LGPL|MIT)"))))' "$lock_file" >/dev/null || {
    echo "plugin effective-license inventory is incomplete" >&2
    exit 1
}
jq -e '(.packages == ["lumina-audited"]) and ([.audit.recipe_allowlist[] | select(. == "gstreamer-1.0" or . == "gst-plugins-base-1.0" or . == "gst-plugins-good-1.0" or . == "gst-plugins-bad-1.0" or . == "gst-libav-1.0" or . == "pipewire" or . == "ffmpeg")] | length == 7)' "$lock_file" >/dev/null || {
    echo "direct audited recipe closure is incomplete" >&2
    exit 1
}
jq -e '
    (.audit.recipe_allowlist | sort | unique) == ([.components[].recipe] | sort | unique) and
    (all(.audit.recipe_allowlist[]; . != "lumina-audited")) and
    (.audit.recipe_metadata | map(.recipe) | sort | unique) == (.audit.recipe_allowlist | sort | unique)
' "$lock_file" >/dev/null || {
    echo "recipe allowlist and recipe metadata must equal the unique fetched component recipes" >&2
    exit 1
}

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
    [[ "$package" == lumina-audited && "${package_roots[*]}" == "bin etc lib libexec share" ]] || {
        echo "unexpected archive layout roots for $package" >&2
        exit 1
    }
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

overlay_dir="$repo_root/vendor/cerbero-overlay"
overlay_config="$overlay_dir/config/lumina-audited.cbc"
[[ -d "$overlay_dir" && -f "$overlay_config" ]] || fail "audited Cerbero overlay is missing"
bad_gpl_patch="$overlay_dir/patches/gst-plugins-bad-1.0-disable-gpl.patch"
[[ -f "$bad_gpl_patch" ]] || fail "gst-plugins-bad GPL patch is missing"
base_minimal_patch="$overlay_dir/patches/gst-plugins-base-1.0-minimal.patch"
good_minimal_patch="$overlay_dir/patches/gst-plugins-good-1.0-minimal.patch"
bad_no_gpl_deps_patch="$overlay_dir/patches/gst-plugins-bad-1.0-no-gpl-deps.patch"
bad_minimal_patch="$overlay_dir/patches/gst-plugins-bad-1.0-minimal.patch"
openssl_no_ca_patch="$overlay_dir/patches/openssl-no-ca-certificates.patch"
[[ -f "$base_minimal_patch" && -f "$good_minimal_patch" &&
   -f "$bad_no_gpl_deps_patch" && -f "$bad_minimal_patch" &&
   -f "$openssl_no_ca_patch" ]] || {
    fail "minimal recipe patches are missing"
}
overlay_package="$overlay_dir/packages/lumina-audited.package"
[[ -f "$overlay_package" ]] || fail "audited Cerbero package is missing"
cp -a -- "$overlay_package" "$cerbero_dir/packages/lumina-audited.package"
while IFS= read -r overlay_recipe; do
    [[ -n "$overlay_recipe" ]] || continue
    overlay_recipe_file="$overlay_dir/recipes/$overlay_recipe.recipe"
    [[ -f "$overlay_recipe_file" ]] || fail "missing repo-owned overlay recipe: $overlay_recipe"
    cp -a -- "$overlay_recipe_file" "$cerbero_dir/recipes/$overlay_recipe.recipe"
done < <(jq -er '.audit.recipe_metadata[] | select(.overlay == true) | .recipe' "$lock_file")
grep -Eq '^[[:space:]]*files[[:space:]]*=' "$overlay_package" || fail "audited package has no direct files list"
if grep -Eq '^[[:space:]]*deps[[:space:]]*=' "$overlay_package"; then
    fail "audited private package must not depend on an upstream package"
fi
for package_file_spec in "${package_file_specs[@]}"; do
    grep -F "'$package_file_spec'" "$overlay_package" >/dev/null || {
        fail "audited package is missing lock file category: $package_file_spec"
    }
done

grep -F "tarball_checksum = '$gstreamer_sha'" "$cerbero_dir/recipes/gstreamer-1.0.recipe" >/dev/null
grep -F "tarball_checksum = '$libav_sha'" "$cerbero_dir/recipes/gst-libav-1.0.recipe" >/dev/null
grep -F "tarball_checksum = '6636f2c2289ceda52c4aba971338c81e2b5780d3381bd3673c1c116ec87587c3'" \
    "$cerbero_dir/recipes/gst-plugins-bad-1.0.recipe" >/dev/null || {
    fail "pinned gst-plugins-bad recipe checksum is not 1.28.6"
}
grep -F "tarball_checksum = '0ba699c7c6c66f4ba640be78cb38a24715add9683f3e3a199f5369dc5a4f04ac'" \
    "$cerbero_dir/recipes/gst-plugins-base-1.0.recipe" >/dev/null || {
    fail "pinned gst-plugins-base recipe checksum is not 1.28.6"
}
grep -F "tarball_checksum = 'b0c620a4b18b6ee931b4c43bbf1760d308666dc37f730a7e7f1ad327e59ce2df'" \
    "$cerbero_dir/recipes/gst-plugins-good-1.0.recipe" >/dev/null || {
    fail "pinned gst-plugins-good recipe checksum is not 1.28.6"
}
patch --directory "$cerbero_dir" --batch --forward --fuzz=0 --strip=1 <"$base_minimal_patch" >/dev/null || {
    fail "could not apply the pinned gst-plugins-base minimal patch"
}
patch --directory "$cerbero_dir" --batch --forward --fuzz=0 --strip=1 <"$good_minimal_patch" >/dev/null || {
    fail "could not apply the pinned gst-plugins-good minimal patch"
}
patch --directory "$cerbero_dir" --batch --forward --fuzz=0 --strip=1 <"$bad_gpl_patch" >/dev/null || {
    fail "could not apply the pinned gst-plugins-bad GPL patch"
}
patch --directory "$cerbero_dir" --batch --forward --fuzz=0 --strip=1 <"$bad_no_gpl_deps_patch" >/dev/null || {
    fail "could not apply the pinned gst-plugins-bad dependency patch"
}
patch --directory "$cerbero_dir" --batch --forward --fuzz=0 --strip=1 <"$bad_minimal_patch" >/dev/null || {
    fail "could not apply the pinned gst-plugins-bad minimal plugin patch"
}
patch --directory "$cerbero_dir" --batch --forward --fuzz=0 --strip=1 <"$openssl_no_ca_patch" >/dev/null || {
    fail "could not apply the pinned OpenSSL CA dependency patch"
}
grep -F "'adaptivedemux2': 'enabled'" "$cerbero_dir/recipes/gst-plugins-good-1.0.recipe" >/dev/null || {
    fail "minimal gst-plugins-good recipe lost adaptivedemux2"
}
grep -F "'soup': 'enabled'" "$cerbero_dir/recipes/gst-plugins-good-1.0.recipe" >/dev/null || {
    fail "minimal gst-plugins-good recipe lost HTTPS support"
}
grep -F "'vpx': 'enabled'" "$cerbero_dir/recipes/gst-plugins-good-1.0.recipe" >/dev/null || {
    fail "minimal gst-plugins-good recipe lost VP9 support"
}
grep -F "'opus': 'enabled'" "$cerbero_dir/recipes/gst-plugins-base-1.0.recipe" >/dev/null || {
    fail "minimal gst-plugins-base recipe lost Opus support"
}
grep -F "'gpl': 'disabled'" "$cerbero_dir/recipes/gst-plugins-bad-1.0.recipe" >/dev/null || {
    fail "patched gst-plugins-bad recipe does not disable its actual GPL option"
}
grep -F "'hls': 'enabled'" "$cerbero_dir/recipes/gst-plugins-bad-1.0.recipe" >/dev/null || {
    fail "minimal gst-plugins-bad recipe lost HLS"
}
grep -F "'hls-crypto': 'openssl'" "$cerbero_dir/recipes/gst-plugins-bad-1.0.recipe" >/dev/null || {
    fail "minimal gst-plugins-bad recipe lost OpenSSL HLS crypto"
}
if grep -F "'bz2': 'enabled'" "$cerbero_dir/recipes/gst-plugins-bad-1.0.recipe" >/dev/null; then
    fail "minimal gst-plugins-bad recipe still enables the bzip2 plugin"
fi
for forbidden_bad_hook in \
    "enable_plugin('nvcodec'" "enable_plugin('curl'" \
    "enable_plugin('svtjpegxs'" "enable_plugin('unixfd'" \
    "enable_plugin('msdk'" "enable_plugin('rsvg'"; do
    if grep -F "$forbidden_bad_hook" "$cerbero_dir/recipes/gst-plugins-bad-1.0.recipe" >/dev/null; then
        fail "minimal gst-plugins-bad recipe still enables $forbidden_bad_hook"
    fi
done
if grep -F "'gpl': 'enabled'" "$cerbero_dir/recipes/gst-plugins-bad-1.0.recipe" >/dev/null; then
    fail "patched gst-plugins-bad recipe still enables GPL"
fi
if grep -F "'codecs_gpl_restricted'" "$cerbero_dir/recipes/gst-plugins-bad-1.0.recipe" >/dev/null; then
    fail "patched gst-plugins-bad recipe still requests GPL-restricted codecs"
fi
grep -F "tarball_checksum = '$pipewire_sha'" "$cerbero_dir/recipes/pipewire.recipe" >/dev/null || {
    fail "PipeWire recipe checksum disagrees with lock"
}
grep -F "'gstreamer': 'enabled'" "$cerbero_dir/recipes/pipewire.recipe" >/dev/null || {
    fail "PipeWire recipe does not enable its actual GStreamer option"
}
grep -F "'spa-plugins': 'enabled'" "$cerbero_dir/recipes/pipewire.recipe" >/dev/null || {
    fail "PipeWire recipe does not enable its actual SPA option"
}
for pipewire_option in \
    docs man examples tests installed_tests gstreamer-device-provider \
    libsystemd logind selinux systemd-system-service systemd-user-service \
    bluez5 jack v4l2 pipewire-alsa pipewire-jack pipewire-v4l2 dbus libcamera \
    udev libpulse sdl2 sndfile libmysofa roc avahi echo-cancel-webrtc libusb \
    raop lv2 x11 x11-xfixes libcanberra readline gsettings compress-offload \
    pw-cat pw-cat-ffmpeg ffmpeg libffado opus gsettings-pulse-schema ebur128 \
    fftw onnxruntime flatpak; do
    grep -F "'$pipewire_option': 'disabled'" "$cerbero_dir/recipes/pipewire.recipe" >/dev/null || {
        fail "PipeWire recipe does not disable its audited $pipewire_option option"
    }
done
grep -F "'session-managers': []" "$cerbero_dir/recipes/pipewire.recipe" >/dev/null || {
    fail "PipeWire recipe does not disable session managers"
}
grep -F "'jack-devel': False" "$cerbero_dir/recipes/pipewire.recipe" >/dev/null || {
    fail "PipeWire recipe has an invalid jack-devel option value"
}
grep -F "'legacy-rtkit': False" "$cerbero_dir/recipes/pipewire.recipe" >/dev/null || {
    fail "PipeWire recipe leaves legacy rtkit enabled"
}
grep -F "'gstreamer-1.0', 'gst-plugins-base-1.0'" "$cerbero_dir/recipes/pipewire.recipe" >/dev/null || {
    fail "PipeWire recipe is missing its GStreamer plugin build dependencies"
}
ffmpeg_recipe="$cerbero_dir/recipes/ffmpeg.recipe"
ffmpeg_recipe_version=$(sed -n "s/^[[:space:]]*version = '\([^']*\)'$/\1/p" "$ffmpeg_recipe")
ffmpeg_recipe_sha=$(sed -n "s/^[[:space:]]*tarball_checksum = '\([^']*\)'$/\1/p" "$ffmpeg_recipe")
# Cerbero's pinned 7.1 Meson recipe has no separate gpl key; upstream's
# default is disabled. If a future pinned recipe adds the key, it must state
# disabled explicitly below.
[[ "$ffmpeg_recipe_version" == "$ffmpeg_version" ]] || fail "Cerbero FFmpeg recipe version disagrees with lock"
[[ "$ffmpeg_recipe_sha" == "$ffmpeg_sha" ]] || fail "Cerbero FFmpeg recipe checksum disagrees with lock"
grep -F "url = 'https://ffmpeg.org/releases/%(name)s-%(version)s.tar.xz'" "$ffmpeg_recipe" >/dev/null || {
    fail "Cerbero FFmpeg recipe source URL disagrees with lock"
}
grep -F "licenses = [License.LGPLv2_1Plus]" "$ffmpeg_recipe" >/dev/null || {
    fail "Cerbero FFmpeg recipe is not LGPLv2.1+"
}
grep -F "'nonfree': 'disabled'" "$ffmpeg_recipe" >/dev/null || {
    fail "Cerbero FFmpeg recipe does not disable nonfree code"
}
grep -F "'version3': 'disabled'" "$ffmpeg_recipe" >/dev/null || {
    fail "Cerbero FFmpeg recipe does not disable version 3 code"
}
if grep -Eq "['\"]gpl['\"][[:space:]]*:[[:space:]]*['\"]enabled['\"]" "$ffmpeg_recipe"; then
    fail "Cerbero FFmpeg recipe enables GPL code"
fi
if grep -Eq "['\"]gpl['\"][[:space:]]*:" "$ffmpeg_recipe" &&
   ! grep -Eq "['\"]gpl['\"][[:space:]]*:[[:space:]]*['\"]disabled['\"]" "$ffmpeg_recipe"; then
    fail "Cerbero FFmpeg recipe has an unreviewed GPL setting"
fi
if grep -Eiq 'x264|libx264' "$ffmpeg_recipe"; then
    fail "Cerbero FFmpeg recipe names an external x264 input"
fi

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

# Verify every fetched recipe against the reviewed post-overlay closure. This
# catches a stale Cerbero archive before fetch-package can add an unreviewed
# dependency; the lock records final source URLs/checksums and Linux deps.
while IFS=$'\t' read -r recipe expected_version expected_url expected_sha expected_deps expected_platform; do
    recipe_file="$cerbero_dir/recipes/$recipe.recipe"
    [[ -f "$recipe_file" ]] || fail "pinned recipe is missing: $recipe"
    facts=$(recipe_facts "$recipe_file")
    actual_sha=$(jq -r '.sha256 // empty' <<<"$facts")
    [[ "$actual_sha" == "$expected_sha" ]] || fail "recipe checksum disagrees with lock: $recipe"
    actual_version=$(jq -r '.version // empty' <<<"$facts")
    if [[ -n "$actual_version" && "$actual_version" != "$expected_version" ]]; then
        fail "recipe version disagrees with lock: $recipe"
    fi
    [[ "$(jq -c '.deps' <<<"$facts")" == "$expected_deps" ]] || {
        fail "recipe dependency closure disagrees with lock: $recipe"
    }
    [[ "$(jq -c '.platform_deps' <<<"$facts")" == "$expected_platform" ]] || {
        fail "recipe Linux platform dependency closure disagrees with lock: $recipe"
    }
done < <(jq -er '.audit.recipe_metadata[] | [.recipe, .version, .source_url, .sha256, (.deps | tojson), (.platform_deps | tojson)] | @tsv' "$lock_file")

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

grep -F "deps = ['zlib']" "$cerbero_dir/recipes/openssl.recipe" >/dev/null || {
    fail "patched OpenSSL recipe still pulls the generated ca-certificates recipe"
}

variant_csv=$(IFS=,; printf '%s' "${variants[*]}")
cerbero=("$cerbero_dir/cerbero-uninstalled" --non-interactive \
    -c "$cerbero_dir/config/linux.config" -c "$overlay_config" -v "$variant_csv")
cerbero_show_config=
if ! cerbero_show_config=$("${cerbero[@]}" show-config); then
    fail "Cerbero show-config failed"
fi
parse_show_config_value() {
    local key=$1 result_var=$2 parsed
    if ! parsed=$(awk -v key="$key" '
        $0 ~ "^[[:space:]]*" key "[[:space:]]*:" {
            count++
            value = $0
            sub("^[[:space:]]*" key "[[:space:]]*:[[:space:]]*", "", value)
            sub("[[:space:]]*$", "", value)
            if (value == "") {
                invalid = 1
            } else {
                print value
            }
        }
        END { exit (count == 1 && !invalid) ? 0 : 1 }
    ' <<<"$cerbero_show_config"); then
        fail "Cerbero show-config did not provide exactly one nonempty $key value"
    fi
    printf -v "$result_var" '%s' "$parsed"
}

show_config_local_sources=
parse_show_config_value local_sources show_config_local_sources
[[ "$show_config_local_sources" == /* ]] || {
    fail "Cerbero show-config local_sources is not absolute"
}
normalized_work_dir=$(realpath -m -- "$work_dir") || fail "could not normalize build work directory"
if ! source_cache_root=$(realpath -m -- "$show_config_local_sources"); then
    fail "could not normalize Cerbero local_sources"
fi
case "$source_cache_root/" in
    "$normalized_work_dir/"*)
        ;;
    *)
        fail "Cerbero local_sources escapes the build work directory"
        ;;
esac
show_config_system_priority=
parse_show_config_value system_recipes_priority show_config_system_priority
[[ "$show_config_system_priority" == -1 ]] || {
    fail "Cerbero system recipe priority is not -1"
}
show_config_allow_system=
parse_show_config_value allow_system_recipes show_config_allow_system
[[ "$show_config_allow_system" == True ]] || {
    fail "Cerbero system recipes are not available"
}
mkdir -p "$source_cache_root"
snapshot_source_cache() {
    local destination=$1 file relative digest
    : >"$destination"
    while IFS= read -r -d '' file; do
        relative=${file#"$source_cache_root"/}
        digest=$(sha256sum -- "$file" | awk '{print $1}')
        printf '%s\t%s\n' "$relative" "$digest" >>"$destination"
    done < <(find -P "$source_cache_root" -type f -print0 | sort -z)
}

# Bootstrap may populate its own source cache. Snapshot it first, then audit
# package fetches by locked component SHA. A component may reuse a
# bootstrap-fetched archive, but every newly introduced/changed file must
# still be one of the reviewed component archives; basenames are labels only.
"${cerbero[@]}" fetch-bootstrap --system=no --toolchains=no --build-tools=yes --jobs=2
bootstrap_source_snapshot="$work_dir/bootstrap-source-cache.tsv"
snapshot_source_cache "$bootstrap_source_snapshot"
before_runtime_source_snapshot="$work_dir/before-runtime-source-cache.tsv"
cp -a -- "$bootstrap_source_snapshot" "$before_runtime_source_snapshot"
for package in "${packages[@]}"; do
    "${cerbero[@]}" fetch-package "$package" --deps --jobs=2
done
[[ "${packages[*]}" == lumina-audited ]] || fail "only lumina-audited may be fetched"
after_runtime_source_snapshot="$work_dir/after-runtime-source-cache.tsv"
snapshot_source_cache "$after_runtime_source_snapshot"
new_runtime_source_snapshot="$work_dir/new-runtime-source-cache.tsv"
awk -F '\t' 'NR == FNR { before[$1] = $2; next }
    !($1 in before) || before[$1] != $2 { print }' \
    "$before_runtime_source_snapshot" "$after_runtime_source_snapshot" >"$new_runtime_source_snapshot"

expected_source_rows="$work_dir/expected-source-rows.tsv"
# Cerbero BaseTarball caches under <local_sources>/<package_name>/<tarball_name>;
# a recipe may rewrite tarball_name. Ownership is SHA-based; URL basenames
# are output labels only.
jq -er '.components[] | [.name, .recipe, .sha256, (.source_url | split("/") | last)] | @tsv' \
    "$lock_file" >"$expected_source_rows"
runtime_source_matches="$work_dir/runtime-source-matches.tsv"
: >"$runtime_source_matches"
declare -A component_archive=()
declare -A component_source_rel=()
diagnose_source_matches() {
    local component_name=$1 component_recipe=$2 expected_sha=$3 expected_filename=$4
    local match_count=$5 snapshot=$6 source_relative source_sha top_cache_dir basename
    local candidate_count=0
    printf 'source cache match failure: component=%s recipe=%s expected_sha=%s match_count=%s\n' \
        "$component_name" "$component_recipe" "$expected_sha" "$match_count" >&2
    while IFS=$'\t' read -r source_relative source_sha; do
        [[ -n "$source_relative" ]] || continue
        top_cache_dir=${source_relative%%/*}
        basename=${source_relative##*/}
        if [[ "$source_sha" == "$expected_sha" ||
              "$top_cache_dir" == "$component_recipe" ||
              "$top_cache_dir" == "$component_recipe-"* ||
              "$basename" == "$expected_filename" ]]; then
            if ((candidate_count < 20)); then
                printf '  candidate %s\t%s\n' "$source_relative" "$source_sha" >&2
            fi
            ((candidate_count += 1))
        fi
    done <"$snapshot"
    if ((candidate_count == 0)); then
        echo '  candidate <none>' >&2
    elif ((candidate_count > 20)); then
        printf '  ... %s additional candidate(s) omitted\n' "$((candidate_count - 20))" >&2
    fi
}
while IFS=$'\t' read -r component_name component_recipe component_sha component_filename; do
    [[ -n "$component_name" && -n "$component_recipe" && -n "$component_sha" && -n "$component_filename" ]] || {
        fail "component source metadata is incomplete"
    }
    mapfile -t source_matches < <(awk -F '\t' -v expected_sha="$component_sha" \
        '($2 == expected_sha) { print $1 }' "$after_runtime_source_snapshot")
    match_count=${#source_matches[@]}
    if ((match_count != 1)); then
        diagnose_source_matches "$component_name" "$component_recipe" "$component_sha" \
            "$component_filename" "$match_count" "$after_runtime_source_snapshot"
        fail "Cerbero cache does not contain exactly one audited source archive for $component_name"
    fi
    source_relative=${source_matches[0]}
    component_archive["$component_name"]="$source_cache_root/$source_relative"
    component_source_rel["$component_name"]="archives/$component_recipe/$component_filename"
    printf '%s\t%s\t%s\t%s\t%s\n' \
        "$component_name" "$component_recipe" "$component_sha" "$component_filename" "$source_relative" \
        >>"$runtime_source_matches"
done <"$expected_source_rows"

# Any newly fetched file not matched above is an unreviewed source input.
while IFS=$'\t' read -r source_relative source_sha; do
    [[ -n "$source_relative" ]] || continue
    awk -F '\t' -v expected_sha="$source_sha" -v expected_path="$source_relative" \
        '$3 == expected_sha && $5 == expected_path { found = 1 } END { exit(found ? 0 : 1) }' \
        "$runtime_source_matches" || fail "Cerbero fetched an unmatched runtime source file: $source_relative"
done <"$new_runtime_source_snapshot"
"${cerbero[@]}" bootstrap --system=no --toolchains=no --build-tools=yes --offline --assume-yes --jobs=2

package_dir="$work_dir/packages"
mkdir -p "$package_dir"
for package in "${packages[@]}"; do
    mkdir -p "$package_dir/$package"
    "${cerbero[@]}" package "$package" \
        --artifact=tarball --compress-method=xz --no-split --no-devel --offline --jobs=2 \
        --output-dir "$package_dir/$package"
done
all_package_tarballs_list="$work_dir/all-package-tarballs"
find -P "$package_dir" -type f -name '*.tar.xz' -print0 >"$all_package_tarballs_list"
mapfile -d '' -t all_package_tarballs <"$all_package_tarballs_list"
[[ ${#all_package_tarballs[@]} -eq 1 ]] || fail "Cerbero emitted more than the lumina-audited package"

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
[[ "$package_count" == 1 ]] || fail "expected exactly one lock package, got $package_count"
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

# Inspect the built plugin metadata in an isolated process. Declared lock
# licenses are provenance, not evidence: an actual GPL or unknown plugin is a
# hard failure even if its filename is absent from the matrix.
plugin_license_tsv="$work_dir/plugin-effective-license.tsv"
: >"$plugin_license_tsv"
inspect_plugin_license() {
    local plugin_file=$1
    local plugin_name=$2
    local report license normalized owner source_url
    owner=$(jq -r --arg path "vendor/linux-x86_64/$archive_runtime_libdir/gstreamer-1.0/$plugin_file" '
        first(.file_ownership[] | select(any(.prefixes[]; . as $prefix | ($path | startswith($prefix)))) | .component) // empty
    ' "$lock_file")
    [[ -n "$owner" ]] || fail "bundled plugin has no package/component owner: $plugin_file"
    source_url=$(jq -r --arg owner "$owner" 'first(.components[] | select(.name == $owner) | .source_url) // empty' "$lock_file")
    [[ -n "$source_url" ]] || fail "bundled plugin owner has no source URL: $plugin_file"
    report=$("$launcher" "$runtime_root/bin/gst-inspect-1.0" "$plugin_name" 2>/dev/null) || {
        fail "gst-inspect could not load bundled plugin $plugin_file"
    }
    license=$(sed -n 's/^[[:space:]]*License:[[:space:]]*//p' <<<"$report" | sed -n '1p')
    normalized=$(tr '[:upper:]' '[:lower:]' <<<"$license")
    [[ -n "$license" && "$normalized" != unknown* ]] || {
        fail "bundled plugin has unknown effective license: $plugin_file"
    }
    if [[ "${normalized//lgpl/}" == *gpl* ]]; then
        fail "bundled plugin has GPL effective license: $plugin_file ($license)"
    fi
    printf '%s\t%s\t%s\t%s\n' "$plugin_file" "$license" "$owner" "$source_url" >>"$plugin_license_tsv"
}
while IFS= read -r -d '' plugin_path; do
    plugin_file=${plugin_path#"$plugin_dir"/}
    plugin_name=${plugin_file#libgst}
    plugin_name=${plugin_name%%.so*}
    inspect_plugin_license "$plugin_file" "$plugin_name"
done < <(find -P "$plugin_dir" -type f -name 'libgst*.so*' -print0 | sort -z)
jq -r '.audit.plugin_allowlist[] | [.element, .filename, .owner_component, .license] | @tsv' "$lock_file" |
    while IFS=$'\t' read -r element filename owner declared_license; do
        actual_license=$(awk -F '\t' -v file="$filename" '$1 == file {print $2; exit}' "$plugin_license_tsv")
        [[ -n "$actual_license" ]] || fail "allowlisted plugin was not inspected: $filename"
        normalized_actual=$(tr '[:upper:]' '[:lower:]' <<<"$actual_license")
        [[ "${normalized_actual//lgpl/}" != *gpl* ]] || {
            fail "allowlisted plugin is GPL despite lock metadata: $element"
        }
        [[ "$owner" != "PipeWire" || "$actual_license" == *MIT* || "$actual_license" == *X11* ]] || {
            fail "PipeWire GStreamer plugin is not MIT/X11: $actual_license"
        }
    done
for forbidden in "${forbidden_components[@]}"; do
    if find -P "$runtime_root" -iname "*$forbidden*" -print -quit | grep -q .; then
        fail "forbidden component is present in the runtime: $forbidden"
    fi
done

# A package category is intentionally direct, but Cerbero may still install
# helper plugins from the selected LGPL recipe categories. Every such plugin
# was inspected above and remains subject to the same forbidden-component check.

is_allowed_system_elf() {
    local candidate=$1 allowed_name
    for allowed_name in "${system_elf_allowlist[@]}"; do
        [[ "$candidate" == "$allowed_name" ]] && return 0
    done
    return 1
}

# Walk DT_NEEDED recursively. Internal dependencies must resolve inside the
# bundle; only the explicit glibc/loader/GPU/display ABI contract may be
# external. Audio, PipeWire, VA-API, and other user-space libraries are
# required to resolve from the bundle.
closure_tsv="$work_dir/elf-closure.tsv"
: >"$closure_tsv"
elf_queue="$work_dir/elf-queue"
find -P "$runtime_root" -type f -print0 |
    while IFS= read -r -d '' file; do
        if readelf -h "$file" >/dev/null 2>&1; then
            printf '%s\n' "$file"
        fi
    done | sort >"$elf_queue"
declare -A seen_elf=()
while IFS= read -r elf; do
    [[ -n "$elf" ]] || continue
    [[ -n "${seen_elf[$elf]:-}" ]] && continue
    seen_elf["$elf"]=1
    relative_elf=${elf#"$runtime_root"/}
    while IFS= read -r needed; do
        [[ -n "$needed" ]] || continue
        internal_path=$(find -P "$runtime_root" -name "$needed" -print -quit)
        if [[ -n "$internal_path" ]]; then
            printf '%s\t%s\tbundled\n' "$relative_elf" "$needed" >>"$closure_tsv"
            if [[ -z "${seen_elf[$internal_path]:-}" ]]; then
                printf '%s\n' "$internal_path" >>"$elf_queue"
            fi
        elif is_allowed_system_elf "$needed"; then
            printf '%s\t%s\tsystem-abi\n' "$relative_elf" "$needed" >>"$closure_tsv"
        else
            fail "ELF DT_NEEDED dependency is outside bundle/ABI allowlist: $relative_elf -> $needed"
        fi
    done < <(readelf -d "$elf" 2>/dev/null | sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p')
done <"$elf_queue"

sort -t $'\t' -k1,1 -k2,2 -k3,3 "$closure_tsv" -o "$closure_tsv"
closure_json=$(jq -Rn '[inputs | split("\t") | {object: .[0], needed: .[1], scope: .[2]}]' <"$closure_tsv")

license_dir="$bundle/licenses"
mkdir -p "$license_dir"
# Cerbero packages normally carry these under share; retain every applicable
# license/notice text and add the overlay's policy text verbatim.
if [[ -d "$runtime_root/share" ]]; then
    while IFS= read -r -d '' license_file; do
        relative_license=${license_file#"$runtime_root/share"/}
        mkdir -p "$license_dir/share/$(dirname -- "$relative_license")"
        cp -a -- "$license_file" "$license_dir/share/$relative_license"
    done < <(find -P "$runtime_root/share" -type f \( -iname '*copying*' -o -iname '*license*' -o -iname '*notice*' \) -print0)
fi
extract_license() {
    local archive=$1
    local member_pattern=$2
    local destination=$3
    local temporary="$work_dir/license.$RANDOM"
    if [[ "$archive" == *.zip ]]; then
        unzip -p "$archive" "$member_pattern" >"$temporary" 2>/dev/null
    else
        tar -xOf "$archive" --wildcards "$member_pattern" >"$temporary" 2>/dev/null
    fi || {
        rm -f -- "$temporary"
        fail "license text is missing from $archive: $member_pattern"
    }
    [[ -s "$temporary" ]] || {
        rm -f -- "$temporary"
        fail "license text is empty in $archive: $member_pattern"
    }
    chmod 0644 -- "$temporary"
    cp -a -- "$temporary" "$destination"
    rm -f -- "$temporary"
}

while IFS=$'\t' read -r component_name license_member license_output; do
    [[ -n "$component_name" && -n "$license_member" && -n "$license_output" ]] || {
        fail "component license metadata is incomplete"
    }
    archive_path=${component_archive[$component_name]:-}
    [[ -f "$archive_path" ]] || fail "component archive is missing from source cache: $component_name"
    extract_license "$archive_path" "$license_member" "$license_dir/$license_output"
done < <(jq -er '.components[] | . as $component |
    (($component.license_members // [{member: $component.license_member, output: $component.license_output}])[] |
     [$component.name, .member, .output] | @tsv)' "$lock_file")
mkdir -p "$license_dir/overlay"
cp -a -- "$overlay_dir/LICENSE.md" "$license_dir/overlay/LICENSE.md"
jq -n --argjson components "$(jq -c '.components' "$lock_file")" \
    --argjson declared "$(printf '%s\n' "${license_texts[@]}" | jq -R . | jq -s .)" \
    '{components: $components, declared_texts: $declared, note: "Source archives carry the complete upstream texts; runtime copies are retained when packaged."}' \
    >"$license_dir/metadata.json"

cat >"$runtime_root/VERSION" <<EOF
GSTREAMER_VERSION=$gstreamer_version
CERBERO_TAG=$cerbero_tag
CERBERO_COMMIT=$cerbero_commit
PIPEWIRE_VERSION=$pipewire_version
TARGET=linux-x86_64
GLIBC_FLOOR=$target_glibc
EOF
cat >"$bundle/NOTICE" <<'EOF'
This audited runtime contains only lock-approved LGPL-compatible GStreamer
plugins and the explicitly documented system ABI closure. H.264/AAC use the
software avdec_h264/avdec_aac fallback when VA-API is unavailable. This build
does not include gst-plugins-ugly, x264, GPL, nonfree, or unknown components.
The corresponding-source archive contains every audited runtime source
archive at a normalized recipe path, the pinned Cerbero archive, and the
Cerbero overlay/policy files. Bootstrap tool sources are excluded.
EOF

# Normalize the generated vendor tree before hashing so the tree digest covers
# the exact files that will be packed.
find "$runtime_root" -exec touch -h -d '@0' {} +
(cd "$bundle" && find vendor -type f -print0 | sort -z |
    while IFS= read -r -d '' file; do sha256sum "$file"; done) >"$bundle/tree.sha256"
tree_sha=$(sha256sum "$bundle/tree.sha256" | awk '{ print $1 }')
file_inventory=$(cd "$bundle" &&
    find . -type f ! -name inventory.json -print0 | sort -z |
    while IFS= read -r -d '' file; do
        path=${file#./}
        owner=$(jq -r --arg path "$path" '
            first(.file_ownership[] | select(any(.prefixes[]; . as $prefix | ($path | startswith($prefix)))) | .component) // empty
        ' "$lock_file")
        if [[ "$path" == *.so || "$path" == *.so.* ]]; then
            [[ -n "$owner" ]] || fail "bundled shared library has no component owner: $path"
        fi
        printf '%s\t%s\t%s\n' "$path" "$(sha256sum "$file" | awk '{ print $1 }')" "${owner:-metadata}"
    done | jq -Rn '[inputs | split("\t") | {path: .[0], sha256: .[1], component: .[2]}]')
plugin_inventory=$(jq -c '.audit.plugin_allowlist' "$lock_file")
component_inventory=$(jq -c '.components' "$lock_file")
closure_json=$(jq -Rn '[inputs | split("\t") | {object: .[0], needed: .[1], scope: .[2]}]' <"$closure_tsv")
actual_plugin_inventory=$(jq -Rn '[inputs | select(length > 0) | split("\t") | {filename: .[0], license: .[1], component: .[2], source_url: .[3]}]' <"$plugin_license_tsv")
jq -n \
    --arg version "$gstreamer_version" \
    --arg commit "$cerbero_commit" \
    --arg pipewire "$pipewire_version" \
    --arg tree_sha "$tree_sha" \
    --argjson variants "$(printf '%s\n' "${variants[@]}" | jq -R . | jq -s .)" \
    --argjson components "$component_inventory" \
    --argjson package_files "$(jq -c '.audit.package_files' "$lock_file")" \
    --argjson plugins "$plugin_inventory" \
    --argjson files "$file_inventory" \
    --argjson closure "$closure_json" \
    --argjson actual_plugins "$actual_plugin_inventory" \
    --argjson system_abi "$(printf '%s\n' "${system_elf_allowlist[@]}" | jq -R . | jq -s .)" \
    '{schema_version: 2, gstreamer_version: $version, pipewire_version: $pipewire, cerbero_commit: $commit, tree_sha256: $tree_sha, packages: ["lumina-audited"], package_files: $package_files, variants: $variants, components: $components, plugin_effective_license: $plugins, actual_plugin_license: $actual_plugins, bundled_files: $files, recursive_dt_needed: $closure, system_abi_allowlist: $system_abi, policy: {gpl: false, nonfree: false, unknown: false, ugly: false, h264_aac: "avdec_h264/avdec_aac"}}' \
    >"$bundle/runtime-manifest.json"

source_bundle_root="$work_dir/source-bundle"
mkdir -p "$source_bundle_root/archives" "$source_bundle_root/overlay" "$source_bundle_root/cerbero"
while IFS=$'\t' read -r component_name component_recipe component_sha component_filename source_relative; do
    archive_path=${component_archive[$component_name]:-}
    source_path=${component_source_rel[$component_name]:-}
    [[ -f "$archive_path" && -n "$source_path" ]] || fail "audited source mapping is incomplete: $component_name"
    mkdir -p "$source_bundle_root/$(dirname -- "$source_path")"
    cp -a -- "$archive_path" "$source_bundle_root/$source_path"
done <"$runtime_source_matches"
cp -a -- "$overlay_dir/." "$source_bundle_root/overlay/"
cp -a -- "$cerbero_archive" "$source_bundle_root/cerbero/"
jq -n --arg runtime_tree_sha "$tree_sha" \
    --arg pipewire "$pipewire_version" \
    --arg cerbero_url "$cerbero_archive_url" \
    --arg cerbero_sha "$cerbero_archive_sha" \
    --argjson components "$component_inventory" \
    --argjson archives "$(jq -c '[.components[] | . as $component | {name, recipe, version, source_url, sha256, filename: (.source_url | split("/") | last), path: ("archives/" + .recipe + "/" + (.source_url | split("/") | last))}]' "$lock_file")" \
    '{schema_version: 2, runtime_tree_sha256: $runtime_tree_sha, pipewire_version: $pipewire, components: $components, archives: $archives, cerbero_archive: {source_url: $cerbero_url, sha256: $cerbero_sha, path: "cerbero/cerbero.tar.gz"}, source_kind: "exact audited runtime archives at normalized recipe paths plus pinned Cerbero archive and repository overlay/patches"}' \
    >"$source_bundle_root/source-manifest.json"
find "$source_bundle_root" -exec touch -h -d '@0' {} +
source_artifact_name=gstreamer-runtime-linux-x86_64.sources.tar.xz
XZ_OPT='-T2 -6' tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
    -cJf "$output_dir/$source_artifact_name" -C "$source_bundle_root" .
(cd "$output_dir" && sha256sum "$source_artifact_name" >"$source_artifact_name.sha256")

# Normalize metadata after writing the manifest; vendor contents and tree.sha256
# are unchanged, so tree_sha still describes the final vendor tree.
find "$bundle" -maxdepth 1 -exec touch -h -d '@0' {} +

artifact_name=gstreamer-runtime-linux-x86_64.tar.xz
XZ_OPT='-T2 -6' tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
    -cJf "$output_dir/$artifact_name" -C "$bundle" .
(cd "$output_dir" && sha256sum "$artifact_name" >"$artifact_name.sha256")
printf '%s  tree\n' "$tree_sha" >"$output_dir/$artifact_name.tree.sha256"
echo "created $output_dir/$artifact_name"
