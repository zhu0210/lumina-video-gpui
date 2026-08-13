# Deterministic fixtures

Run `./fixtures/generate.sh` from the repository root. The script uses only
the local `ffmpeg`, `ffprobe`, and `sha256sum` commands; all media comes from
fixed `lavfi` test sources and no network access is needed. Generated media is
written below `fixtures/generated/` (ignored by git). `fixtures/PROVENANCE`
records the tool versions, stream probes, and SHA-256 for every generated file.

## Matrix

| Fixture | Expected coverage |
| --- | --- |
| `generated/h264-aac.mp4` | 320x180 H.264 video, AAC-LC stereo, 2 seconds |
| `generated/vp9-opus.mkv` | 320x180 VP9 video, Opus stereo, 2 seconds |
| `generated/dual-aac.mkv` | 320x180 H.264 video, two AAC-LC stereo tracks, 2 seconds |
| `generated/hls-vod/index.m3u8` | H.264/AAC VOD playlist with `#EXT-X-ENDLIST` |
| `generated/hls-event/index.m3u8` | H.264/AAC EVENT snapshot with no `#EXT-X-ENDLIST` |

The dual-track file checks container/stream discovery only: this revision has
no public audio-track selection API. The EVENT directory is a deterministic
finite snapshot, not a network service or an assertion of live reconnect
semantics.

## Public harness seam

After generating fixtures, run the high-level example with a local path, for
example:

```bash
# On a headless Linux host, disable the native audio sink for this probe.
EGUI_VID_FAKE_AUDIO=1 cargo run -p lumina-video-core --example fixture_harness -- fixtures/generated/h264-aac.mp4
```

On Linux the example opens the source through the public
`ZeroCopyGStreamerDecoder` implementation of the public
`VideoDecoderBackend` trait and gives it to `CorePlayer::with_decoder`. Other
platforms use `CorePlayer::new` so platform decoder selection remains inside
the crate. The output observes only public behavior: state, metadata, frame
PTS/dimensions/format, playback and audio controls, initialization errors, and
the public CPU/native-surface distinction exposed by `DecodedFrame`.

The future `MediaSession`, `CapabilityTier`, and complete `FrameRealization`
contracts are not present in this checkout. The harness therefore reports
native-surface realization as a partial public observation and labels complete
realization unavailable; it does not invent those APIs or use private
GStreamer elements.
