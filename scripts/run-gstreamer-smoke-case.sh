#!/usr/bin/env bash
set -euo pipefail

source_uri=${1:?source URI}
runtime=${RUNTIME_ROOT:?RUNTIME_ROOT}
if [[ -n "${RUNTIME_ARCHIVE:-}" ]]; then
    mkdir -p /runtime
    tar -xJf "$RUNTIME_ARCHIVE" -C /runtime
    runtime=/runtime/vendor/linux-x86_64
fi
runtime_libdir=${RUNTIME_LIBDIR:?RUNTIME_LIBDIR}
runtime_home=$(mktemp -d "/tmp/lumina-home-${CASE_NAME}.XXXXXX")
runtime_cache=$(mktemp -d "/tmp/lumina-cache-${CASE_NAME}.XXXXXX")
trap 'rm -rf "$runtime_home" "$runtime_cache" /runtime' EXIT
export HOME="$runtime_home" XDG_CACHE_HOME="$runtime_cache"
[[ -n "${SOURCE_CERT:-}" ]] && export G_TLS_CA_FILE="$SOURCE_CERT" SSL_CERT_FILE="$SOURCE_CERT"
unset LD_LIBRARY_PATH GST_PLUGIN_PATH_1_0 GST_PLUGIN_SYSTEM_PATH_1_0 GST_PLUGIN_PATH GST_PLUGIN_SYSTEM_PATH
unset GST_PLUGIN_SCANNER_1_0 GST_PLUGIN_SCANNER GST_REGISTRY_1_0 GST_REGISTRY GST_REGISTRY_REUSE_PLUGIN_SCANNER
lib_dir="$runtime/$runtime_libdir"
plugin_dir="$lib_dir/gstreamer-1.0"
scanner="$runtime/libexec/gstreamer-1.0/gst-plugin-scanner"
launcher="$runtime/bin/lumina-gstreamer-runtime"
[[ -x "$launcher" && -x "$runtime/bin/gst-inspect-1.0" && -x "$runtime/bin/gst-launch-1.0" ]] || exit 1
[[ -d "$plugin_dir" && -x "$scanner" ]] || exit 1

contract=$("$launcher" env)
grep -Fx "GST_PLUGIN_PATH_1_0=$plugin_dir" <<<"$contract" >/dev/null
grep -Fx 'GST_PLUGIN_SYSTEM_PATH_1_0=' <<<"$contract" >/dev/null
grep -Fx 'GST_PLUGIN_PATH=' <<<"$contract" >/dev/null
grep -Fx 'GST_PLUGIN_SYSTEM_PATH=' <<<"$contract" >/dev/null
grep -Fx "GST_PLUGIN_SCANNER_1_0=$scanner" <<<"$contract" >/dev/null
grep -Fx 'GST_PLUGIN_SCANNER=' <<<"$contract" >/dev/null
grep -Fx 'GST_REGISTRY=' <<<"$contract" >/dev/null
grep -Fx 'GST_REGISTRY_REUSE_PLUGIN_SCANNER=no' <<<"$contract" >/dev/null
grep -Fx "LD_LIBRARY_PATH=$lib_dir" <<<"$contract" >/dev/null

"$launcher" "$runtime/bin/gst-inspect-1.0" --version | grep -F "$EXPECTED_GSTREAMER_VERSION" >/dev/null
for element in $REQUIRED_ELEMENTS; do
    "$launcher" "$runtime/bin/gst-inspect-1.0" "$element" >/dev/null
done
registry_path=$("$launcher" bash -c 'printf "%s" "$GST_REGISTRY_1_0"')
case "$registry_path/" in "$runtime/"*|/app/*) exit 1 ;; esac
[[ "$registry_path" == "$XDG_CACHE_HOME"/*/gstreamer-1.0.registry ]] || exit 1

if [[ "$source_uri" == audio: ]]; then
    "$launcher" "$runtime/bin/gst-launch-1.0" -e audiotestsrc num-buffers=20 ! audioconvert ! audioresample ! fakesink
elif [[ "$source_uri" == *hls-live* ]]; then
    set +e
    timeout 15 "$launcher" "$runtime/bin/gst-launch-1.0" -e playbin3 uri="$source_uri" video-sink=fakesink audio-sink=fakesink
    status=$?
    set -e
    [[ "$status" == 0 || "$status" == 124 ]] || exit "$status"
else
    "$launcher" "$runtime/bin/gst-launch-1.0" -e playbin3 uri="$source_uri" video-sink=fakesink audio-sink=fakesink
fi
[[ -f "$registry_path" ]] || exit 1
