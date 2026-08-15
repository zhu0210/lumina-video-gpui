# Audited GStreamer runtime inputs

`gstreamer-1.0.lock.json` is schema 2 and pins every moving input used by the
runtime build: GStreamer 1.28.6, gst-libav/FFmpeg 7.1, zlib, PipeWire 1.6.8,
Cerbero, Ubuntu 24.04/glibc 2.39, the exact `norust,nogi,nounwind,alsa,pulse,va` variants,
and Freedesktop 25.08 Flatpak refs.

The zlib recipe/component source remains zlib.net provenance; the formal
acquisition URL is the lock-pinned official GStreamer mirror with the same
filename and SHA, pre-seeded into Cerbero's resolved local source cache before
fetch.

The Cerbero lock uses the official GitHub mirror, tag object
`78666745b34b6245a85510ac47a03a5033af4711`, peeled commit
`59548269f4fd0f701818f0bafdb102959ec81e65`, commit-addressed codeload URL,
SHA-256 `1874c5ed8b67612ca0370e5a8c7b25420ed98f0176425aa427eb1461293a82d3`,
and root `cerbero-59548269f4fd0f701818f0bafdb102959ec81e65`.

PipeWire 1.6.8 is a lightweight direct tag, with no tag-signature claim.
Official GitLab and GitHub direct refs both resolve to commit
`b741e0c74f5436f0c925f7741140db0efd32cf4e` and expose no peeled ref. The lock
acquires the GitHub codeload archive at that commit with root
`pipewire-b741e0c74f5436f0c925f7741140db0efd32cf4e` and SHA-256
`a78762e007a604846fc16c83979ef15bbb69dba1a2923c32bb6e2f18fafa343f`;
discovery compares its `meson.build`, `COPYING`, and `LICENSE` bytes with the
official GitLab commit raw files and requires them to match.

`nogi`/`nounwind` and the empty GStreamer bash-completion list are deliberate:
introspection, unwind, and shell-completion inputs are not needed for playback
and would add build-only, non-runtime, or runtime-link inputs.

The lock also records verified archive roots; explicit Cerbero root
normalization is limited to ALSA, PulseAudio, and PipeWire, with no runtime
inventory change.
`audit.overlay_inputs` is the single SHA-256 inventory of the 16 regular
repo-owned Cerbero control files under config, packages, recipes, and patches;
the build and discovery scripts reject path-set or byte mismatches.
PulseAudio's lock pins the tag/object/commit, archive URL/SHA/root. The
official API reports a PGP signature on the annotated tag. After
acquisition and before use/build, the scripts verify the archive root and the
script-fixed `meson.build`/`LGPL` byte hashes against locked commit facts.
GitHub-generated archive byte drift fails closed against that SHA and requires
an explicit lock review/update.

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
./scripts/discover-gstreamer-lock.sh \
  --cerbero-dir /path/to/cerbero \
  --cerbero-archive /path/to/cerbero.tar.gz \
  --pipewire-archive /path/to/pipewire.tar.gz \
  --pulseaudio-archive /path/to/pulseaudio.tar.gz
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
