#!/usr/bin/env bash
set -euo pipefail

# Data-driven clean-container matrix. The container receives only the locked
# runtime and deterministic fixtures, with a fresh registry for every case.
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
artifact=${1:-}
lock_file=${2:-$repo_root/vendor/gstreamer-1.0.lock.json}
fixtures_dir=${3:-$repo_root/fixtures/generated}

[[ -n "$artifact" ]] || {
    echo "Usage: $0 RUNTIME.tar.xz [LOCK] [FIXTURES_DIR]" >&2
    exit 2
}
for command_name in docker jq realpath sha256sum find python3 openssl; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done
artifact=$(realpath "$artifact")
lock_file=$(realpath "$lock_file")
fixtures_dir=$(realpath "$fixtures_dir")
[[ -f "$artifact" ]] || { echo "artifact not found: $artifact" >&2; exit 1; }
[[ -f "$lock_file" ]] || { echo "lock not found: $lock_file" >&2; exit 1; }
[[ -d "$fixtures_dir" ]] || {
    echo "fixtures not found; run ./fixtures/generate.sh first: $fixtures_dir" >&2
    exit 1
}

builder_image=$(jq -er '.builder.image' "$lock_file")
expected_version=$(jq -er '.gstreamer.version' "$lock_file")
runtime_libdir=$(jq -er '.artifact.archive_layout.runtime_libdir' "$lock_file")
required_elements=$(jq -er '[.audit.plugin_allowlist[].element] | join(" ")' "$lock_file")
[[ "$builder_image" =~ ^ubuntu@sha256:[[:xdigit:]]{64}$ ]] || exit 1
[[ "$runtime_libdir" == lib/x86_64-linux-gnu ]] || exit 1

for fixture in h264-aac.mp4 vp9-opus.mkv vp9-opus.webm dual-aac.mkv hls-vod/index.m3u8 hls-live/index.m3u8; do
    [[ -f "$fixtures_dir/$fixture" ]] || {
        echo "missing matrix fixture: $fixtures_dir/$fixture" >&2
        exit 1
    }
done

server_pid=''
server_dir=''
cleanup() {
    [[ -n "$server_pid" ]] && kill "$server_pid" >/dev/null 2>&1 || true
    [[ -n "$server_pid" ]] && wait "$server_pid" >/dev/null 2>&1 || true
    [[ -n "$server_dir" ]] && rm -rf "$server_dir"
}
trap cleanup EXIT

start_http() {
    local port=$1
    python3 -m http.server "$port" --bind 127.0.0.1 --directory "$fixtures_dir" >/dev/null 2>&1 &
    server_pid=$!
}

start_https() {
    local port=$1
    server_dir=$(mktemp -d "${TMPDIR:-/tmp}/lumina-gst-https.XXXXXX")
    openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
        -keyout "$server_dir/key.pem" -out "$server_dir/cert.pem" >/dev/null 2>&1
    python3 - "$fixtures_dir" "$port" "$server_dir/cert.pem" "$server_dir/key.pem" <<'PY' &
import functools
import http.server
import ssl
import sys

directory, port, cert, key = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
server = http.server.ThreadingHTTPServer(
    ("127.0.0.1", port),
    functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory),
)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
PY
    server_pid=$!
}

run_case() {
    local case_name=$1 source_uri=$2 network_mode=$3
    local cache_name="${case_name//[^A-Za-z0-9]/_}"
    local -a cert_mount=()
    [[ -n "$server_dir" ]] && cert_mount=(-v "$server_dir/cert.pem:/input/cert.pem:ro")
    docker run --rm --pull=always --network "$network_mode" \
        --env "EXPECTED_GSTREAMER_VERSION=$expected_version" \
        --env "RUNTIME_LIBDIR=$runtime_libdir" \
        --env "REQUIRED_ELEMENTS=$required_elements" \
        --env "CASE_NAME=$cache_name" \
        -v "$artifact:/input/runtime.tar.xz:ro" \
        -v "$fixtures_dir:/input/fixtures:ro" \
        "${cert_mount[@]}" \
        "$builder_image" /bin/bash -s -- "$source_uri" <<'EOF'
set -euo pipefail
source_uri=$1
runtime_home=$(mktemp -d "/tmp/lumina-home-${CASE_NAME}.XXXXXX")
runtime_cache=$(mktemp -d "/tmp/lumina-cache-${CASE_NAME}.XXXXXX")
trap 'rm -rf "$runtime_home" "$runtime_cache" /runtime' EXIT
export HOME="$runtime_home" XDG_CACHE_HOME="$runtime_cache"
[[ -f /input/cert.pem ]] && export G_TLS_CA_FILE=/input/cert.pem SSL_CERT_FILE=/input/cert.pem
unset LD_LIBRARY_PATH GST_PLUGIN_PATH_1_0 GST_PLUGIN_SYSTEM_PATH_1_0 GST_PLUGIN_PATH GST_PLUGIN_SYSTEM_PATH
unset GST_PLUGIN_SCANNER_1_0 GST_PLUGIN_SCANNER GST_REGISTRY_1_0 GST_REGISTRY GST_REGISTRY_REUSE_PLUGIN_SCANNER

mkdir -p /runtime
tar -xJf /input/runtime.tar.xz -C /runtime
runtime=/runtime/vendor/linux-x86_64
launcher="$runtime/bin/lumina-gstreamer-runtime"
lib_dir="$runtime/$RUNTIME_LIBDIR"
plugin_dir="$lib_dir/gstreamer-1.0"
scanner="$runtime/libexec/gstreamer-1.0/gst-plugin-scanner"
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
case "$registry_path/" in "$runtime/"*) exit 1 ;; esac
[[ "$registry_path" == "$XDG_CACHE_HOME"/*/gstreamer-1.0.registry ]] || exit 1

if [[ "$source_uri" == audio: ]]; then
    "$launcher" "$runtime/bin/gst-launch-1.0" -e audiotestsrc num-buffers=20 ! audioconvert ! audioresample ! fakesink
elif [[ "$source_uri" == *hls-live* ]]; then
    set +e
    timeout 15 "$launcher" "$runtime/bin/gst-launch-1.0" -e playbin3 \
        uri="$source_uri" video-sink=fakesink audio-sink=fakesink
    status=$?
    set -e
    [[ "$status" == 0 || "$status" == 124 ]] || exit "$status"
else
    "$launcher" "$runtime/bin/gst-launch-1.0" -e playbin3 \
        uri="$source_uri" video-sink=fakesink audio-sink=fakesink
fi
[[ -f "$registry_path" ]] || exit 1
EOF
}

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
start_http "$port"
matrix_cases=(
    "mp4-h264-aac|file:///input/fixtures/h264-aac.mp4|none"
    "matroska-vp9-opus|file:///input/fixtures/vp9-opus.mkv|none"
    "webm-vp9-opus|file:///input/fixtures/vp9-opus.webm|none"
    "dual-track-mkv|file:///input/fixtures/dual-aac.mkv|none"
    "audio|audio:|none"
    "hls-vod-http|http://127.0.0.1:$port/hls-vod/index.m3u8|host"
    "hls-live-http|http://127.0.0.1:$port/hls-live/index.m3u8|host"
)
for matrix_case in "${matrix_cases[@]}"; do
    IFS='|' read -r case_name source_uri network_mode <<<"$matrix_case"
    run_case "$case_name" "$source_uri" "$network_mode"
done
kill "$server_pid" >/dev/null 2>&1 || true
wait "$server_pid" >/dev/null 2>&1 || true
server_pid=''
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
start_https "$port"
matrix_cases=(
    "hls-vod-https|https://127.0.0.1:$port/hls-vod/index.m3u8|host"
    "hls-live-https|https://127.0.0.1:$port/hls-live/index.m3u8|host"
)
for matrix_case in "${matrix_cases[@]}"; do
    IFS='|' read -r case_name source_uri network_mode <<<"$matrix_case"
    run_case "$case_name" "$source_uri" "$network_mode"
done

echo "audited GStreamer runtime smoke matrix passed"
