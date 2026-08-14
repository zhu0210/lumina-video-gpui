# Lumina audited Cerbero overlay

This small, repository-owned overlay is copied into the pinned Cerbero tree by
`scripts/build-gstreamer-runtime.sh`. `lumina-audited` is the only package
requested by Cerbero and the only package artifact accepted by the builder; its
file list names direct core/base/good/bad/libav, PipeWire, FFmpeg, codec, and
system-library recipe categories. No upstream GStreamer package artifact is
emitted.
This follows Cerbero's documented private-package API: a private package uses
direct `files` categories and omits package-level `deps`.
There is no same-name `lumina-audited` recipe; the package file is the sole
custom output and the lock recipe allowlist contains only fetched recipes.

The lock is the authority for the exact variants and plugin set. The overlay
keeps Rust off and carries applied patches against the pinned 1.28.6 recipe
files: base/good are reduced to the required playback/network/audio paths,
bad disables every unused optional plugin while retaining HLS, and the strict
GPL patch changes the actual `gst-plugins-bad-1.0.recipe`
`meson_options['gpl']` value to `disabled`. PipeWire 1.6.8 is a real Meson
recipe: it enables `gstreamer` and SPA/client libraries while disabling the
daemon, session managers, docs, tests, examples, BlueZ, JACK, and ALSA/V4L2
extras. The builder checks the patched recipe text and the pinned Cerbero
FFmpeg recipe's LGPL license and disabled `nonfree`/`version3` options, plus
absence of GPL/x264 settings, before it fetches the closure. GPL, ugly, and
x264 features are rejected by the build audit.

References: the [Cerbero build guide](https://gstreamer.freedesktop.org/documentation/installing/building-from-source-using-cerbero.html),
[GStreamer Cerbero deployment guide](https://gstreamer.freedesktop.org/documentation/deploying/multiplatform-using-cerbero.html),
[PipeWire 1.6.8 Meson options](https://raw.githubusercontent.com/PipeWire/pipewire/1.6.8/meson_options.txt),
[GStreamer HLS demuxer](https://gstreamer.freedesktop.org/documentation/adaptivedemux2/hlsdemux2.html),
and [FFmpeg licensing](https://ffmpeg.org/legal.html).
