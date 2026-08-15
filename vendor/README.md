# Audited GStreamer runtime inputs

`gstreamer-1.0.lock.json` is schema 2 and pins every moving input used by the
runtime build: GStreamer 1.28.6, gst-libav/FFmpeg 7.1, zlib, PipeWire 1.6.8,
Cerbero, Ubuntu 24.04/glibc 2.39, the exact `norust,nogi,nounwind,alsa,pulse,va` variants,
and Freedesktop 25.08 Flatpak refs.

`nogi`/`nounwind` and the empty GStreamer bash-completion list are deliberate:
introspection, unwind, and shell-completion inputs are not needed for playback
and would add build-only, non-runtime, or runtime-link inputs.

The lock also records verified archive roots; explicit Cerbero root
normalization is limited to ALSA, PulseAudio, and PipeWire, with no runtime
inventory change.
`audit.overlay_inputs` is the single SHA-256 inventory of the 16 regular
repo-owned Cerbero control files under config, packages, recipes, and patches;
the build and discovery scripts reject path-set or byte mismatches.

The audited closure is intentionally narrow: only the lock's direct recipe
categories are built into the single `lumina-audited` package, with exactly 21
GStreamer plugin shared objects selected by repo-owned recipe lists. The matrix
plugin allowlist records effective license/source, while the build and audit
run isolated `gst-inspect-1.0` over every bundled plugin. GPL/nonfree/version-3
FFmpeg options, gst-plugins-ugly, x264, and unknown licenses are rejected.
H.264/AAC software fallback is `avdec_h264`/`avdec_aac`. ALSA, PulseAudio,
PipeWire, and libva user-space libraries are bundled; only glibc/loader and
explicit GPU/display driver ABI names remain external.
The 29-component closure also bundles libsndfile because PulseAudio's
client-common ABI links it unconditionally; its external/MPEG/optional codec
features are disabled, so no recursive codec closure is added.

Discovery is the only moving-metadata path:

```bash
./scripts/discover-gstreamer-lock.sh
```

The formal build reads only the lock, seeds the Cerbero source cache from the
locked URLs/checksums, then runs bootstrap/package offline with two workers:

```bash
./scripts/build-gstreamer-runtime.sh \
  --lock vendor/gstreamer-1.0.lock.json \
  --output dist/gstreamer-runtime
```

It emits one audited `gstreamer-runtime-linux-x86_64.tar.xz` for standalone and
Flatpak, plus the corresponding-source `*.sources.tar.xz`, full applicable
license texts/metadata, and deterministic `runtime-manifest.json` inventory.
The manifest records every bundled-file SHA, plugin effective license/source,
and recursive `DT_NEEDED` closure. Use `scripts/audit-gstreamer-runtime.sh`
before publishing.

`vendor/cerbero-overlay` is deliberately small and reviewable. It contains the
Cerbero `localconf.cbc`, the direct `lumina-audited` package, the real PipeWire
recipe, and the applied base/good/bad recipe patches; the entire overlay is
copied into corresponding-source.tar.xz. The custom package has no same-name
recipe: its direct `files` categories drive the reviewed closure.

The launcher establishes private `LD_LIBRARY_PATH`, GStreamer plugin/scanner
paths, and an external registry/cache. It never falls back to host plugins.
Flatpak uses only `--socket=pulseaudio`; PipeWire is a native closure/presence
check, not a broad `XDG_RUNTIME_DIR` passthrough. Flatpak build provenance
records actual OSTree commits but does not permanently pin a user's runtime.

See [docs/GSTREAMER-RUNTIME.md](../docs/GSTREAMER-RUNTIME.md) for official
upstream links and the copyright/patent/legal-review boundary.
