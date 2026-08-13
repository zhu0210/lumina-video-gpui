# Deterministic fixtures

Run `./fixtures/generate.sh` from the repository root. The script uses only
the local `ffmpeg`, `ffprobe`, and `sha256sum` commands; all media comes from
fixed `lavfi` test sources and no network access is needed. Generated media is
written below `fixtures/generated/` (ignored by git). `fixtures/PROVENANCE`
records the tool versions, stream probes, and SHA-256 for every generated file.

## Matrix

| Fixture | Expected contents |
| --- | --- |
| `generated/h264-aac.mp4` | 320x180 H.264 video, AAC-LC stereo, 2 seconds |
| `generated/vp9-opus.mkv` | 320x180 VP9 video, Opus stereo, 2 seconds |
| `generated/dual-aac.mkv` | 320x180 H.264 video, two AAC-LC stereo tracks, 2 seconds |
| `generated/hls-vod/index.m3u8` | H.264/AAC VOD playlist with `#EXT-X-ENDLIST` |
| `generated/hls-live/index.m3u8` | H.264/AAC EVENT snapshot with no `#EXT-X-ENDLIST` |

The dual-track file checks container/stream discovery and a successful public
in-session audio switch by stable GStreamer stream id. The harness then requests
a missing id and verifies an explicit nonterminal preflight failure, the prior
selection, and frame continuity. Sent-selection failure plus confirmed rollback
is covered by the deterministic native-frame unit seam, not by this fixture.
The EVENT directory is a deterministic finite snapshot, not a network service
or an assertion of live reconnect semantics.

## Public harness seam

After generating fixtures, run the high-level example with a local path, for
example:

```bash
# The harness uses GStreamer's deterministic fake audio sink.
cargo run -p lumina-video-gst --example fixture_harness -- fixtures/generated/vp9-opus.mkv
cargo run -p lumina-video-gst --example fixture_harness -- fixtures/generated/dual-aac.mkv
```

The deterministic HTTP redirect, HTTPS-success, invalid-certificate, range,
and unreachable-endpoint checks use the same generated HLS VOD fixture through
the public `GstMediaSession` seam. Run the complete network check with:

```bash
./fixtures/generate.sh
cargo test -p lumina-video-gst public_network_vod_handles_redirect_https_and_typed_failures -- --nocapture
```

The test creates its loopback server with `std::net::TcpListener` and its
short-lived certificate with the existing Rustls/rcgen test dependencies. The
trusted CA is passed to that test decoder instance only; normal constructors
continue to use the platform trust store. If generated fixtures are absent,
the network test reports the required generate command and skips rather than
failing unrelated unit-test runs.

On Linux the example opens the source through the public `GstMediaSession`
seam. It polls exactly one event per tick, checks metadata and autoplay state,
asserts that frames are owned CPU memory, observes the bounded drop-oldest
counter, exercises pause/play, mute/volume, seek, and EOS replay, and verifies
that GStreamer owns the audio path. The harness keeps the adapter's default
two-second worker operation bound. No GPU or private GStreamer element is
required.
