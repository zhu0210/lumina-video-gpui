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
pipewire_url="https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/1.6.8/pipewire-1.6.8.tar.gz"
pipewire_tag_commit=$(git ls-remote --tags https://gitlab.freedesktop.org/pipewire/pipewire.git \
    'refs/tags/1.6.8^{}' | awk 'NR == 1 { print $1 }')
[[ "$pipewire_tag_commit" =~ ^[[:xdigit:]]{40}$ ]] || {
    echo "official PipeWire 1.6.8 peeled commit was not found" >&2
    exit 1
}
archive_tmp=$(mktemp)
pipewire_tmp=$(mktemp)
mkdir -p "$(dirname -- "$lock_file")"
lock_tmp=$(mktemp "${lock_file}.XXXXXX")
cleanup() {
    rm -f "$archive_tmp" "$pipewire_tmp" "$lock_tmp"
}
trap cleanup EXIT
curl -fL --retry 3 --max-filesize 50000000 "$cerbero_archive_url" -o "$archive_tmp"
cerbero_archive_sha=$(sha256sum "$archive_tmp" | awk '{ print $1 }')
curl -fL --retry 3 --max-filesize 10000000 "$pipewire_url" -o "$pipewire_tmp"
pipewire_sha=$(sha256sum "$pipewire_tmp" | awk '{ print $1 }')
[[ "$pipewire_sha" =~ ^[[:xdigit:]]{64}$ ]] || {
    echo "official PipeWire checksum is not a SHA-256 digest" >&2
    exit 1
}
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
ffmpeg_recipe=$(tar -xOf "$archive_tmp" --wildcards '*/recipes/ffmpeg.recipe')
ffmpeg_version=$(sed -n "s/^[[:space:]]*version = '\([^']*\)'$/\1/p" <<<"$ffmpeg_recipe")
ffmpeg_sha=$(sed -n "s/^[[:space:]]*tarball_checksum = '\([^']*\)'$/\1/p" <<<"$ffmpeg_recipe")
ffmpeg_url="https://ffmpeg.org/releases/ffmpeg-${ffmpeg_version}.tar.xz"
[[ "$ffmpeg_version" == 7.1 && "$ffmpeg_sha" == 40973d44970dbc83ef302b0609f2e74982be2d85916dd2ee7472d30678a7abe6 ]] || {
    echo "Cerbero FFmpeg recipe is not the approved 7.1 LGPL source" >&2
    exit 1
}
grep -F "'nonfree': 'disabled'" <<<"$ffmpeg_recipe" >/dev/null || {
    echo "Cerbero FFmpeg recipe does not disable nonfree code" >&2
    exit 1
}
grep -F "'version3': 'disabled'" <<<"$ffmpeg_recipe" >/dev/null || {
    echo "Cerbero FFmpeg recipe does not disable version 3 code" >&2
    exit 1
}
if grep -Eq "['\"]gpl['\"][[:space:]]*:[[:space:]]*['\"]enabled['\"]" <<<"$ffmpeg_recipe"; then
    echo "Cerbero FFmpeg recipe enables GPL code" >&2
    exit 1
fi

archive_layout='{
  "source_libdir": "lib/x86_64-linux-gnu",
  "runtime_libdir": "lib/x86_64-linux-gnu",
  "package_roots": {
    "gstreamer-1.0": ["bin", "etc", "lib", "libexec", "share"],
    "gstreamer-1.0-libav": ["lib"]
  }
}'

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
  {"name":"fakesink","filename":"libgstcoreelements.so","purpose":"headless smoke sink"},
  {"name":"matroskademux","filename":"libgstmatroska.so","purpose":"Matroska demux"},
  {"name":"vp9dec","filename":"libgstvpx.so","purpose":"VP9 decoder"},
  {"name":"opusdec","filename":"libgstopus.so","purpose":"Opus decoder"},
  {"name":"hlsdemux","filename":"libgsthls.so","purpose":"HLS demux"},
  {"name":"souphttpsrc","filename":"libgstsoup.so","purpose":"HTTP(S) source"},
  {"name":"alsasink","filename":"libgstalsa.so","purpose":"ALSA presence"},
  {"name":"pulsesink","filename":"libgstpulse.so","purpose":"PulseAudio presence"},
  {"name":"pipewiresink","filename":"libgstpipewire.so","purpose":"PipeWire presence"},
  {"name":"va","filename":"libgstva.so","purpose":"VA-API presence"}
]'

plugin_allowlist='[
  {"element":"playbin3","filename":"libgstplayback.so","source":"gstreamer","license":"LGPL-2.1-or-later"},
  {"element":"qtdemux","filename":"libgstisomp4.so","source":"gst-plugins-good","license":"LGPL-2.1-or-later"},
  {"element":"h264parse","filename":"libgstvideoparsersbad.so","source":"gst-plugins-bad","license":"LGPL-2.1-or-later"},
  {"element":"avdec_h264","filename":"libgstlibav.so","source":"gst-libav/FFmpeg","license":"LGPL-2.1-or-later"},
  {"element":"aacparse","filename":"libgstaudioparsers.so","source":"gst-plugins-base","license":"LGPL-2.1-or-later"},
  {"element":"avdec_aac","filename":"libgstlibav.so","source":"gst-libav/FFmpeg","license":"LGPL-2.1-or-later"},
  {"element":"fakesink","filename":"libgstcoreelements.so","source":"gstreamer","license":"LGPL-2.1-or-later"},
  {"element":"matroskademux","filename":"libgstmatroska.so","source":"gst-plugins-good","license":"LGPL-2.1-or-later"},
  {"element":"vp9dec","filename":"libgstvpx.so","source":"gst-plugins-good","license":"LGPL-2.1-or-later"},
  {"element":"opusdec","filename":"libgstopus.so","source":"gst-plugins-base","license":"LGPL-2.1-or-later"},
  {"element":"hlsdemux","filename":"libgsthls.so","source":"gst-plugins-bad","license":"LGPL-2.1-or-later"},
  {"element":"souphttpsrc","filename":"libgstsoup.so","source":"gst-plugins-good","license":"LGPL-2.1-or-later"},
  {"element":"alsasink","filename":"libgstalsa.so","source":"gst-plugins-good","license":"LGPL-2.1-or-later"},
  {"element":"pulsesink","filename":"libgstpulse.so","source":"gst-plugins-good","license":"LGPL-2.1-or-later"},
  {"element":"pipewiresink","filename":"libgstpipewire.so","source":"gst-plugins-good","license":"LGPL-2.1-or-later"},
  {"element":"va","filename":"libgstva.so","source":"gst-plugins-bad","license":"LGPL-2.1-or-later"}
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
    --arg pipewire_url "$pipewire_url" \
    --arg pipewire_sha "$pipewire_sha" \
    --arg pipewire_commit "$pipewire_tag_commit" \
    --arg ffmpeg_version "$ffmpeg_version" \
    --arg ffmpeg_url "$ffmpeg_url" \
    --arg ffmpeg_sha "$ffmpeg_sha" \
    --arg ubuntu_digest "$ubuntu_digest" \
    --argjson archive_layout "$archive_layout" \
    --argjson required_elements "$required_elements" \
    --argjson plugin_allowlist "$plugin_allowlist" \
    '{
      schema_version: 2,
      gstreamer: {version: $version, source: {url: $gstreamer_url, sha256: $gstreamer_sha, license: "LGPL-2.1-or-later"}},
      sources: {
        gst_libav: {package: "gst-libav-1.0", filename: ("gst-libav-" + $version + ".tar.xz"), url: $libav_url, sha256: $libav_sha, license: "LGPL-2.1-or-later"},
        zlib: {version: $zlib_version, filename: $zlib_filename, url: $zlib_url, sha256: $zlib_sha, license: "Zlib"},
        pipewire: {version: "1.6.8", tag: "1.6.8", tag_commit: $pipewire_commit, url: $pipewire_url, sha256: $pipewire_sha, license: "MIT/LGPL-2.1-or-later", license_url: "https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.6.8/LICENSE"}
      },
      cerbero: {repository: $cerbero_repo, tag: $cerbero_tag, tag_object: $cerbero_tag_object, commit: $cerbero_commit, archive: {url: $cerbero_archive_url, sha256: $cerbero_archive_sha}},
      target: {os: "linux", architecture: "x86_64", distribution: "ubuntu", distribution_version: "24.04", glibc: "2.39"},
      builder: {image: ("ubuntu@" + $ubuntu_digest), platform: "linux/amd64"},
      packages: ["gstreamer-1.0", "gstreamer-1.0-libav"],
      artifact: {type: "tarball", compression: "xz", split: false, archive_layout: $archive_layout},
      variants: ["norust", "alsa", "pulse", "va"],
      audit: {
        recipe_allowlist: ["gstreamer-1.0", "gstreamer-1.0-libav"],
        plugin_allowlist: $plugin_allowlist,
        policy: {
          gst_bad_gpl: false,
          gst_bad_ugly: false,
          gst_libav_ffmpeg_gpl: false,
          gst_libav_ffmpeg_nonfree: false,
          gst_libav_ffmpeg_version3: false,
          software_fallback: {video: "avdec_h264", audio: "avdec_aac"},
          forbidden_components: ["gst-plugins-ugly", "x264"],
          forbidden_licenses: ["GPL", "GPL-2.0", "GPL-3.0", "nonfree", "unknown"]
        },
        system_elf_allowlist: ["ld-linux-x86-64.so.2", "libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1", "libgcc_s.so.1", "libstdc++.so.6", "libasound.so.2", "libpulse.so.0", "libpipewire-0.3.so.0", "libva.so.2", "libva-drm.so.2", "libdrm.so.2", "libEGL.so.1", "libGL.so.1", "libGLX.so.0", "libX11.so.6", "libXext.so.6", "libXfixes.so.3", "libXdamage.so.1", "libXrender.so.1", "libXi.so.6", "libXrandr.so.2", "libXcomposite.so.1", "libxcb.so.1", "libxcb-shm.so.0", "libxcb-render.so.0", "libxcb-randr.so.0", "libxcb-xfixes.so.0", "libwayland-client.so.0", "libwayland-egl.so.1", "libwayland-cursor.so.0", "libz.so.1", "libbz2.so.1.0", "libffi.so.8", "libpcre2-8.so.0", "libfontconfig.so.1", "libfreetype.so.6", "libexpat.so.1", "libpng16.so.16", "libjpeg.so.8", "libcap.so.2", "libmount.so.1", "libselinux.so.1", "libsystemd.so.0", "libdbus-1.so.3", "libudev.so.1", "libcrypt.so.1", "libresolv.so.2"]
      },
      components: [
        {name: "gstreamer", version: $version, source_url: $gstreamer_url, sha256: $gstreamer_sha, license: "LGPL-2.1-or-later"},
        {name: "gst-libav", version: $version, source_url: $libav_url, sha256: $libav_sha, license: "LGPL-2.1-or-later"},
        {name: "FFmpeg", version: $ffmpeg_version, source_url: $ffmpeg_url, sha256: $ffmpeg_sha, license: "LGPL-2.1-or-later (gpl/nonfree/version3 disabled)"},
        {name: "zlib", version: $zlib_version, source_url: $zlib_url, sha256: $zlib_sha, license: "Zlib"},
        {name: "PipeWire", version: "1.6.8", tag: "1.6.8", tag_commit: $pipewire_commit, source_url: $pipewire_url, sha256: $pipewire_sha, license: "MIT/LGPL-2.1-or-later"}
      ],
      license_texts: ["gstreamer/COPYING", "gst-libav/COPYING.LGPL", "FFmpeg/COPYING.LGPLv2.1", "zlib/README", "PipeWire/LICENSE", "overlay/LICENSE.md"],
      flatpak: {
        runtime: "org.freedesktop.Platform", runtime_version: "25.08", runtime_ref: "org.freedesktop.Platform//25.08",
        sdk: "org.freedesktop.Sdk", sdk_version: "25.08", sdk_ref: "org.freedesktop.Sdk//25.08",
        rust_extension: "org.freedesktop.Sdk.Extension.rust-stable", rust_extension_ref: "org.freedesktop.Sdk.Extension.rust-stable//25.08",
        source_metadata: [
          {name: "freedesktop-platform", source_url: "https://github.com/flathub/org.freedesktop.Platform", license: "MIT"},
          {name: "freedesktop-sdk", source_url: "https://gitlab.com/freedesktop-sdk/freedesktop-sdk", license: "LGPL-2.1-or-later"}
        ],
        provenance: "record actual OSTree commits with flatpak info --show-commit; do not pin user runtime"
      },
      required_elements: $required_elements
    }' >"$lock_tmp"
mv -f "$lock_tmp" "$lock_file"
trap - EXIT
rm -f "$archive_tmp" "$pipewire_tmp"
echo "wrote $lock_file for GStreamer $gstreamer_version"
