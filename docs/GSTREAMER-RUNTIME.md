# Audited Linux GStreamer runtime

`vendor/gstreamer-1.0.lock.json` (schema 2) is the only build authority. The
formal workflow fetches only the selected lock-owned Cerbero closure and
builds with the pinned
[Cerbero release](https://gstreamer.freedesktop.org/documentation/installing/building-from-source-using-cerbero.html),
then switches to offline mode for bootstrap, packaging, audit, and artifact
assembly. `lumina-audited` is the only Cerbero package output; its direct file
categories select exactly 21 audited GStreamer plugin shared objects: core
elements, playback/audio/video helpers, MP4/Matroska/HLS/VP9/Opus paths,
ALSA/Pulse/VA/PipeWire, and LGPL FFmpeg. Broad upstream codec categories are
not packaged. All Cerbero jobs use two workers.

The lock fixes 29 runtime components, including GStreamer 1.28.6,
FFmpeg 7.1, the libsoup 3.6.6 HTTPS closure (glib-networking, libproxy,
libpsl, nghttp2, and sqlite3), PipeWire 1.6.8, and the explicitly bundled
audio/VA/DRM user-space libraries. libsndfile is bundled because PulseAudio's
client-common ABI links it unconditionally; its external/MPEG/optional codec
features are disabled, so it adds no recursive codec closure. It also fixes
the Ubuntu builder image, the exact variant set `norust,nogi,nounwind,alsa,pulse,va`, recipe/plugin audit allowlists, and
the system ELF ABI allowlist. `vendor/cerbero-overlay` contains the direct
package, source-backed audio/VA/DRM recipes, PipeWire recipe, and applied
base/good/bad/GStreamer plugin-list, bash-completion, and OpenSSL recipe patches; all are included in the
corresponding-source archive. The build verifies every patch against the
pinned recipe, checks the actual FFmpeg options, and verifies actual plugin
licenses with isolated `gst-inspect-1.0`.
The audit parses each plugin's space-delimited `License` and `Source module`
fields, accepts only raw `LGPL` or `MIT/X11`, and records normalized SPDX values.
Before packaging, the local AST checks provide friendly diagnostics for the
reviewed base/good/bad/GStreamer control sets; they are not the security
boundary. The lock's exact overlay hashes, post-package 21-plugin inventory,
source/license checks, and artifact audit are authoritative.

The lock section `audit.overlay_inputs` contains exactly 16 regular control files
under the repo-owned config/package/recipe/patch directories. Build and
discovery materialize that list and verify every path and SHA-256 before using
any Cerbero input; extra, missing, or tampered controls fail.

The `nogi` and `nounwind` variants disable GObject introspection and unwind
inputs, and the GStreamer recipe's Cerbero bash-completion integration is
disabled. These are not
needed for playback and would add build-only, non-runtime, or runtime-link
inputs to the audited closure.

The lock records the verified archive roots for the seven recipes whose source
trees do not all use Cerbero's default directory names. Explicit
`tarball_dirname` normalization is limited to ALSA, PulseAudio, and PipeWire;
the runtime inventory remains 29 components.

`scripts/discover-gstreamer-lock.sh` is metadata-only: it accepts a local
extracted pinned Cerbero tree and local Cerbero/PipeWire archives, validates
every locked recipe after the repo patches, and queries only small official
tag/checksum endpoints. It refuses to download those archives itself.
The PipeWire archive is addressed to commit
`b741e0c74f5436f0c925f7741140db0efd32cf4e` and byte-locked by its SHA-256.

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

The LGPL configuration is a copyright/license choice for the selected runtime
files, not a conclusion about every source file in an upstream archive.
H.264/AAC patent, codec-licensing, and regional distribution questions remain
outside this technical audit and must be reviewed by the distributor.

## Artifacts and isolation

The build emits one audited `gstreamer-runtime-linux-x86_64.tar.xz` used by
standalone packaging and Flatpak, plus the exact corresponding
`gstreamer-runtime-linux-x86_64.sources.tar.xz`. The source archive contains
exactly one SHA-verified runtime source archive per locked recipe at
`archives/<recipe>/<filename>`, the pinned Cerbero archive, and the repo
overlay; bootstrap tool sources and cache-directory guesses are excluded.
`scripts/audit-gstreamer-runtime.sh` checks archive safety,
manifest hashes, policy flags, closure inventory, and source/runtime
correspondence.

Run the standalone launcher as the process entrypoint:

```text
vendor/linux-x86_64/bin/lumina-gstreamer-runtime /path/to/lumina-video
```

It establishes private library/plugin/scanner paths and a writable registry
under external `XDG_CACHE_HOME` (or `$HOME/.cache`). No host plugin path or
bundle-local registry is accepted. Flatpak builds the demo inside the declared
Freedesktop 25.08 SDK with the rust-stable extension, vendors the lock-pinned
Cargo sources during that SDK build with network granted only to that module;
the subsequent Cargo compile is offline. It exposes only the PulseAudio socket
and does not pass a broad `XDG_RUNTIME_DIR`; PipeWire is checked as native
closure/presence only.
The workflow records the actual OSTree commits used by the build as
provenance, without claiming a permanent user-runtime pin.

## Smoke matrix

The clean-container smoke script runs isolated registries for MP4 H.264/AAC,
Matroska/WebM VP9/Opus, dual-track MKV, audio, HLS VOD/live over loopback HTTP
and HTTPS, and ALSA/Pulse/PipeWire/VA element presence. It does not cover
session semantics or hardware certification; those remain separate scopes.

Primary references: [GStreamer source index](https://gstreamer.freedesktop.org/src/gstreamer/),
[Cerbero deployment guide](https://gstreamer.freedesktop.org/documentation/deploying/multiplatform-using-cerbero.html),
[pinned gst-plugins-base recipe](https://raw.githubusercontent.com/GStreamer/cerbero/1.28.6/recipes/gst-plugins-base-1.0.recipe),
[pinned gst-plugins-good recipe](https://raw.githubusercontent.com/GStreamer/cerbero/1.28.6/recipes/gst-plugins-good-1.0.recipe),
[pinned gst-plugins-bad recipe](https://raw.githubusercontent.com/GStreamer/cerbero/1.28.6/recipes/gst-plugins-bad-1.0.recipe),
[pinned libsoup recipe](https://raw.githubusercontent.com/GStreamer/cerbero/1.28.6/recipes/libsoup.recipe),
[pinned glib-networking recipe](https://raw.githubusercontent.com/GStreamer/cerbero/1.28.6/recipes/glib-networking.recipe),
[pinned libproxy recipe](https://raw.githubusercontent.com/GStreamer/cerbero/1.28.6/recipes/libproxy.recipe),
[pinned nghttp2 recipe](https://raw.githubusercontent.com/GStreamer/cerbero/1.28.6/recipes/nghttp2.recipe),
[pinned sqlite3 recipe](https://raw.githubusercontent.com/GStreamer/cerbero/1.28.6/recipes/sqlite3.recipe),
[FFmpeg configure options](https://ffmpeg.org/ffmpeg-all.html#toc-Advanced-options),
[GStreamer HLS demuxer](https://gstreamer.freedesktop.org/documentation/adaptivedemux2/hlsdemux2.html),
[PipeWire 1.6.8 Meson options](https://raw.githubusercontent.com/PipeWire/pipewire/1.6.8/meson_options.txt),
[PipeWire 1.6.8 tag](https://gitlab.freedesktop.org/pipewire/pipewire/-/tags/1.6.8),
[PipeWire 1.6.8 commit-addressed archive](https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/b741e0c74f5436f0c925f7741140db0efd32cf4e/pipewire-b741e0c74f5436f0c925f7741140db0efd32cf4e.tar.gz),
[PipeWire 1.6.8 documentation](https://docs.pipewire.org/),
[Flatpak runtime documentation](https://docs.flatpak.org/en/latest/available-runtimes.html),
and the [Flathub Freedesktop Platform manifest](https://github.com/flathub/org.freedesktop.Platform).
