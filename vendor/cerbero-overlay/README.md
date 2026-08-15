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
The 29-component closure includes libsndfile 1.2.2 because PulseAudio's
client-common ABI links it unconditionally; its external/MPEG/optional codec
features are disabled, so no recursive codec closure is added.
The Pulse client categories are exact: `libs_lumina` selects top-level
`libpulse`, and `lumina_private` selects the literal
`pulseaudio/libpulsecommon-17.0` file. The common library's lock-authorized private
`lib/x86_64-linux-gnu/pulseaudio` directory is added to runtime loader paths,
without packaging server, modules, or alternate client libraries.

The lock is the authority for the exact variants and plugin set. The overlay
keeps Rust off and carries applied patches against the pinned 1.28.6 recipe
files: repo-owned `files_plugins_lumina` lists select exactly 21 playback,
network, audio, video, HLS, hardware, and fallback plugin shared objects;
base/good/bad are reduced to those lists; bad also exposes exactly four
private runtime libraries (`libgstcodecparsers-1.0`, `libgstcodecs-1.0`,
`libgstmpegts-1.0`, and `libgstva-1.0`) for the selected closure. The strict
GPL patch changes the actual `gst-plugins-bad-1.0.recipe`
`meson_options['gpl']` value to `disabled`. PipeWire 1.6.8 is a real Meson
recipe: it enables `gstreamer` and SPA/client libraries while disabling the
daemon, session managers, docs, tests, examples, BlueZ, JACK, and ALSA/V4L2
extras. The builder checks the patched recipe text and the pinned Cerbero
FFmpeg recipe's LGPL license and disabled `nonfree`/`version3` options, plus
absence of GPL/x264 settings, before it fetches the closure. GPL, ugly, and
x264 features are rejected by the build audit.
The pre-package AST seam provides friendly diagnostics for reviewed plugin
controls and Meson options; it is not the security authority. Lock exact
overlay input hashes, the post-package exact 21-plugin inventory, and the
artifact audit are authoritative.
The lock also names the exact canonical `.so` paths for every selected public
library and the Pulse private common library. GStreamer core/base use literal
`libs_lumina` lists (`libgstreamer-1.0`, `libgstbase-1.0`, `libgstnet-1.0` and
the seven base libraries); controller/check/FFT/RTSP/SDP/GL/app broad-category
spill is not packaged. Build/discovery compare recipe-derived paths, while
package and artifact checks reject any missing or extra canonical shared file.

References: the [Cerbero build guide](https://gstreamer.freedesktop.org/documentation/installing/building-from-source-using-cerbero.html),
[GStreamer Cerbero deployment guide](https://gstreamer.freedesktop.org/documentation/deploying/multiplatform-using-cerbero.html),
[PipeWire 1.6.8 Meson options](https://raw.githubusercontent.com/PipeWire/pipewire/1.6.8/meson_options.txt),
[GStreamer HLS demuxer](https://gstreamer.freedesktop.org/documentation/adaptivedemux2/hlsdemux2.html),
and [FFmpeg licensing](https://ffmpeg.org/legal.html).
