#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
artifact=${1:-}
lock_file=${2:-$repo_root/vendor/gstreamer-1.0.lock.json}
fixture=${3:-$repo_root/fixtures/generated/h264-aac.mp4}

[[ -n "$artifact" ]] || {
    echo "Usage: $0 ARTIFACT [LOCK] [FIXTURE]" >&2
    exit 2
}
for command_name in docker jq realpath; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done
artifact=$(realpath "$artifact")
lock_file=$(realpath "$lock_file")
fixture=$(realpath "$fixture")
[[ -f "$artifact" ]] || { echo "artifact not found: $artifact" >&2; exit 1; }
[[ -f "$lock_file" ]] || { echo "lock not found: $lock_file" >&2; exit 1; }
[[ -f "$fixture" ]] || {
    echo "fixture not found; run ./fixtures/generate.sh first: $fixture" >&2
    exit 1
}

builder_image=$(jq -er '.builder.image' "$lock_file")
[[ "$builder_image" =~ ^ubuntu@sha256:[[:xdigit:]]{64}$ ]] || {
    echo "smoke requires the digest-pinned Ubuntu image from the lock" >&2
    exit 1
}
expected_version=$(jq -er '.gstreamer.version' "$lock_file")
runtime_libdir=$(jq -er '.artifact.archive_layout.runtime_libdir' "$lock_file")
[[ "$runtime_libdir" == lib/x86_64-linux-gnu ]] || {
    echo "smoke requires the locked native multiarch library directory" >&2
    exit 1
}
required_elements=$(jq -r '.required_elements[] | [.name, .filename] | @tsv' "$lock_file")
[[ -n "$required_elements" ]] || { echo "lock has no required smoke elements" >&2; exit 1; }

# The smoke container has no network and receives only the built runtime and
# deterministic MP4 fixture. The artifact launcher owns every runtime
# environment variable; this shell only supplies a clean HOME.
docker run --rm -i --pull=always --network none \
    --env "EXPECTED_GSTREAMER_VERSION=$expected_version" \
    --env "RUNTIME_LIBDIR=$runtime_libdir" \
    --env "REQUIRED_ELEMENTS=$required_elements" \
    -v "$artifact:/input/runtime.tar.gz:ro" \
    -v "$fixture:/input/h264-aac.mp4:ro" \
    "$builder_image" /bin/bash -s <<'EOF'
set -euo pipefail
export LC_ALL=C

runtime_home=$(mktemp -d)
trap 'rm -rf "$runtime_home"' EXIT
export HOME="$runtime_home"
unset XDG_CACHE_HOME
unset LD_LIBRARY_PATH GST_PLUGIN_PATH_1_0 GST_PLUGIN_SYSTEM_PATH_1_0 \
    GST_PLUGIN_PATH GST_PLUGIN_SYSTEM_PATH GST_PLUGIN_SCANNER_1_0 \
    GST_PLUGIN_SCANNER GST_REGISTRY_1_0 GST_REGISTRY \
    GST_REGISTRY_REUSE_PLUGIN_SCANNER

runtime=/runtime/vendor/linux-x86_64
launcher="$runtime/bin/lumina-gstreamer-runtime"
[[ "$RUNTIME_LIBDIR" == lib/x86_64-linux-gnu ]] || exit 1
lib_dir="$runtime/$RUNTIME_LIBDIR"
plugin_dir="$lib_dir/gstreamer-1.0"
scanner="$runtime/libexec/gstreamer-1.0/gst-plugin-scanner"

rm -rf /runtime
mkdir -p /runtime
tar -xzf /input/runtime.tar.gz -C /runtime

[[ -x "$launcher" ]] || exit 1
[[ -x "$runtime/bin/gst-inspect-1.0" ]] || exit 1
[[ -x "$runtime/bin/gst-launch-1.0" ]] || exit 1
[[ -d "$plugin_dir" ]] || exit 1
[[ -x "$scanner" ]] || exit 1
"$launcher" "$runtime/bin/lumina-runtime-probe"

contract=$(
    "$launcher" env
)
grep -Fx "GST_PLUGIN_PATH_1_0=$plugin_dir" <<<"$contract" >/dev/null
grep -Fx 'GST_PLUGIN_SYSTEM_PATH_1_0=' <<<"$contract" >/dev/null
grep -Fx 'GST_PLUGIN_PATH=' <<<"$contract" >/dev/null
grep -Fx 'GST_PLUGIN_SYSTEM_PATH=' <<<"$contract" >/dev/null
grep -Fx "GST_PLUGIN_SCANNER_1_0=$scanner" <<<"$contract" >/dev/null
grep -Fx 'GST_PLUGIN_SCANNER=' <<<"$contract" >/dev/null
grep -Fx 'GST_REGISTRY=' <<<"$contract" >/dev/null
grep -Fx 'GST_REGISTRY_REUSE_PLUGIN_SCANNER=no' <<<"$contract" >/dev/null
grep -Fx "LD_LIBRARY_PATH=$lib_dir" <<<"$contract" >/dev/null
registry_path=$("$launcher" bash -c 'printf "%s" "$GST_REGISTRY_1_0"')
[[ -n "$registry_path" ]] || exit 1
[[ "$(basename -- "$registry_path")" == gstreamer-1.0.registry ]] || exit 1
case "$registry_path/" in
    "$runtime/"*)
        echo "registry path is inside the runtime bundle: $registry_path" >&2
        exit 1
        ;;
esac
registry_dir=$(dirname -- "$registry_path")
[[ -d "$registry_dir" && -w "$registry_dir" ]] || exit 1

version_output=$("$launcher" "$runtime/bin/gst-inspect-1.0" --version)
grep -F "$EXPECTED_GSTREAMER_VERSION" <<<"$version_output" >/dev/null

while IFS=$'\t' read -r element filename; do
    [[ -f "$plugin_dir/$filename" ]] || {
        echo "missing required plugin file: $filename" >&2
        exit 1
    }
    actual=$("$launcher" "$runtime/bin/gst-inspect-1.0" "$element" |
        sed -n 's/^[[:space:]]*Filename[[:space:]]\{1,\}//p' | sed -n '1p')
    [[ "$actual" == "$plugin_dir/$filename" ]] || {
        echo "$element resolved to $actual, expected $plugin_dir/$filename" >&2
        exit 1
    }
done <<<"$REQUIRED_ELEMENTS"

[[ -f "$registry_path" ]] || {
    echo "GStreamer did not create the locked private registry" >&2
    exit 1
}

timeout --signal=TERM --kill-after=5s 60s \
    "$launcher" "$runtime/bin/gst-launch-1.0" -e playbin3 \
    uri=file:///input/h264-aac.mp4 \
    video-sink=fakesink audio-sink=fakesink
EOF
echo "GStreamer runtime smoke passed"
