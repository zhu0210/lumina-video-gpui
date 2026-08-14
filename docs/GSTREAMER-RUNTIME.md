# Audited Linux GStreamer runtime

`vendor/gstreamer-1.0.lock.json` (schema 2) is the only build authority. The
formal workflow fetches the lock-owned sources, builds with the pinned
[Cerbero release](https://gstreamer.freedesktop.org/documentation/installing/building-from-source-using-cerbero.html),
then switches to offline mode for bootstrap, packaging, audit, and artifact
assembly. All Cerbero jobs use two workers.

The lock fixes GStreamer 1.28.6, gst-libav, FFmpeg 7.1, zlib, PipeWire 1.6.8,
the Ubuntu builder image, the exact variant set `norust,alsa,pulse,va`,
recipe/plugin audit allowlists, and the system ELF ABI allowlist.
`vendor/cerbero-overlay` is a small repo-owned `localconf.cbc` plus
policy/package closure; it is included in the corresponding-source archive
rather than being an unreviewed local patch.

## License and codec policy

The audited configuration keeps FFmpeg's `--disable-gpl`,
`--disable-nonfree`, and `--disable-version3` settings and excludes
`gst-plugins-ugly` and x264. H.264 and AAC software fallback is explicitly
`avdec_h264` and `avdec_aac`; VA-API is an optional acceleration path. Plugin
effective licenses, component source URLs/checksums, bundled-file hashes, and
recursive `DT_NEEDED` closure are emitted in `runtime-manifest.json`.

This is a build configuration and provenance record, not legal certification.
Distribution owners must perform their own copyright/license review using the
[GStreamer license information](https://gstreamer.freedesktop.org/documentation/additional/licensing.html),
[FFmpeg legal page](https://ffmpeg.org/legal.html), and the full texts in the
runtime `licenses/` directory and corresponding-source archive. H.264/AAC
patent, regional, and distribution obligations are outside this repository's
technical audit and require separate legal review.

## Artifacts and isolation

The build emits one audited `gstreamer-runtime-linux-x86_64.tar.xz` used by
standalone packaging and Flatpak, plus the exact corresponding
`gstreamer-runtime-linux-x86_64.sources.tar.xz`. The source archive contains
the complete fetched Cerbero source cache, the pinned Cerbero archive, and the
repo overlay. `scripts/audit-gstreamer-runtime.sh` checks archive safety,
manifest hashes, policy flags, closure inventory, and source/runtime
correspondence.

Run the standalone launcher as the process entrypoint:

```text
vendor/linux-x86_64/bin/lumina-gstreamer-runtime /path/to/lumina-video
```

It establishes private library/plugin/scanner paths and a writable registry
under external `XDG_CACHE_HOME` (or `$HOME/.cache`). No host plugin path or
bundle-local registry is accepted. Flatpak uses Freedesktop Platform/Sdk and
`rust-stable` 25.08, exposes only the PulseAudio socket, and does not pass a
broad `XDG_RUNTIME_DIR`; PipeWire is checked as native closure/presence only.
The workflow records the actual OSTree commits used by the build as
provenance, without claiming a permanent user-runtime pin.

## Smoke matrix

The clean-container smoke script runs isolated registries for MP4 H.264/AAC,
Matroska/WebM VP9/Opus, dual-track MKV, audio, HLS VOD/live over loopback HTTP
and HTTPS, and ALSA/Pulse/PipeWire/VA element presence. It does not cover
session semantics or hardware certification; those remain separate scopes.

Primary references: [GStreamer source index](https://gstreamer.freedesktop.org/src/gstreamer/),
[FFmpeg configure options](https://ffmpeg.org/ffmpeg-all.html#toc-Advanced-options),
[PipeWire 1.6.8 tag](https://gitlab.freedesktop.org/pipewire/pipewire/-/tags/1.6.8),
[PipeWire 1.6.8 archive](https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/1.6.8/pipewire-1.6.8.tar.gz),
[PipeWire 1.6.8 documentation](https://docs.pipewire.org/),
[Flatpak runtime documentation](https://docs.flatpak.org/en/latest/available-runtimes.html),
and the [Flathub Freedesktop Platform manifest](https://github.com/flathub/org.freedesktop.Platform).
