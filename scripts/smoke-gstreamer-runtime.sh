#!/usr/bin/env bash
set -euo pipefail

# Data-driven clean-container matrix. The container receives only the locked
# runtime and deterministic fixtures, with a fresh registry for every case.
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
artifact=${1:-}
lock_file=${2:-$repo_root/vendor/gstreamer-1.0.lock.json}
fixtures_dir=${3:-$repo_root/fixtures/generated}
flatpak_app=''
if [[ "$artifact" == --flatpak-app ]]; then
    flatpak_app=${2:-}
    lock_file=${3:-$repo_root/vendor/gstreamer-1.0.lock.json}
    fixtures_dir=${4:-$repo_root/fixtures/generated}
    artifact=''
fi

[[ -n "$artifact" || -n "$flatpak_app" ]] || {
    echo "Usage: $0 RUNTIME.tar.xz [LOCK] [FIXTURES_DIR] | --flatpak-app APP_ID [LOCK] [FIXTURES_DIR]" >&2
    exit 2
}
for command_name in jq realpath sha256sum find python3 openssl; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "missing required command: $command_name" >&2
        exit 1
    }
done
if [[ -n "$flatpak_app" ]]; then
    command -v flatpak >/dev/null 2>&1 || { echo "missing required command: flatpak" >&2; exit 1; }
else
    command -v docker >/dev/null 2>&1 || { echo "missing required command: docker" >&2; exit 1; }
fi
[[ -z "$artifact" ]] || artifact=$(realpath "$artifact")
lock_file=$(realpath "$lock_file")
fixtures_dir=$(realpath "$fixtures_dir")
[[ -n "$flatpak_app" || -f "$artifact" ]] || { echo "artifact not found: $artifact" >&2; exit 1; }
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
        -addext 'subjectAltName=IP:127.0.0.1' \
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
    local case_script="$repo_root/scripts/run-gstreamer-smoke-case.sh"
    if [[ -n "$flatpak_app" ]]; then
        local -a cert_mount=(--filesystem="$fixtures_dir:ro")
        [[ -n "$server_dir" ]] && cert_mount+=(--filesystem="$server_dir:ro")
        flatpak run --user "${cert_mount[@]}" \
            --env=EXPECTED_GSTREAMER_VERSION="$expected_version" \
            --env=RUNTIME_LIBDIR="$runtime_libdir" \
            --env=RUNTIME_ROOT=/app/vendor/linux-x86_64 \
            --env=REQUIRED_ELEMENTS="$required_elements" \
            --env=CASE_NAME="$cache_name" \
            --env=SOURCE_CERT="${server_dir:+$server_dir/cert.pem}" \
            --command=/app/vendor/linux-x86_64/bin/lumina-gstreamer-runtime \
            "$flatpak_app" /bin/bash -s -- "$source_uri" <"$case_script"
    else
        local -a cert_mount=()
        [[ -n "$server_dir" ]] && cert_mount=(-v "$server_dir/cert.pem:/input/cert.pem:ro")
        docker run --rm --network "$network_mode" \
            --env "EXPECTED_GSTREAMER_VERSION=$expected_version" \
            --env "RUNTIME_LIBDIR=$runtime_libdir" \
            --env RUNTIME_ROOT=/runtime/vendor/linux-x86_64 \
            --env RUNTIME_ARCHIVE=/input/runtime.tar.xz \
            --env "REQUIRED_ELEMENTS=$required_elements" \
            --env "CASE_NAME=$cache_name" \
            --env SOURCE_CERT="${server_dir:+/input/cert.pem}" \
            -v "$artifact:/input/runtime.tar.xz:ro" \
            -v "$fixtures_dir:/input/fixtures:ro" \
            "${cert_mount[@]}" \
            "$builder_image" /bin/bash -s -- "$source_uri" <"$case_script"
    fi
}

if [[ -z "$flatpak_app" ]]; then
    # Resolve the immutable image once. Matrix cases never trigger hidden pulls.
    docker pull "$builder_image" >/dev/null
fi

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
start_http "$port"
if [[ -n "$flatpak_app" ]]; then
    file_root="$fixtures_dir"
else
    file_root=/input/fixtures
fi
matrix_cases=(
    "mp4-h264-aac|file://$file_root/h264-aac.mp4|none"
    "matroska-vp9-opus|file://$file_root/vp9-opus.mkv|none"
    "webm-vp9-opus|file://$file_root/vp9-opus.webm|none"
    "dual-track-mkv|file://$file_root/dual-aac.mkv|none"
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
if [[ -n "$flatpak_app" ]]; then
    file_root="$fixtures_dir"
else
    file_root=/input/fixtures
fi
matrix_cases=(
    "hls-vod-https|https://127.0.0.1:$port/hls-vod/index.m3u8|host"
    "hls-live-https|https://127.0.0.1:$port/hls-live/index.m3u8|host"
)
for matrix_case in "${matrix_cases[@]}"; do
    IFS='|' read -r case_name source_uri network_mode <<<"$matrix_case"
    run_case "$case_name" "$source_uri" "$network_mode"
done

echo "audited GStreamer runtime smoke matrix passed"
