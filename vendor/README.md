# Audited GStreamer runtime inputs

`gstreamer-1.0.lock.json` is schema 2 and pins every moving input used by the
runtime build: GStreamer 1.28.6, gst-libav/FFmpeg 7.1, zlib, PipeWire 1.6.8,
Cerbero, Ubuntu 24.04/glibc 2.39, the exact `norust,alsa,pulse,va` variants,
and Freedesktop 25.08 Flatpak refs.

The audited closure is intentionally narrow: only the lock's recipes are
built, and the lock's matrix plugin allowlist records the effective
license/source for every exercised element. Helper plugins that Cerbero's
selected LGPL groups bring along remain inside that recipe closure and are
checked for forbidden components/licenses. GPL/nonfree/version-3 FFmpeg
options, gst-plugins-ugly, x264, and unknown licenses are rejected. H.264/AAC
software fallback is `avdec_h264`/`avdec_aac`. The system ELF allowlist is
limited to the explicit glibc/loader/GPU/audio runtime ABI contract.

Discovery is the only moving-metadata path:

```bash
./scripts/discover-gstreamer-lock.sh
```

The formal build reads only the lock, fetches sources, then runs Cerbero
fetch/bootstrap/package offline with two workers:

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
Cerbero `localconf.cbc`, policy fragment, custom closure marker, and its own
license text; the entire overlay is copied into corresponding-source.tar.xz.

The launcher establishes private `LD_LIBRARY_PATH`, GStreamer plugin/scanner
paths, and an external registry/cache. It never falls back to host plugins.
Flatpak uses only `--socket=pulseaudio`; PipeWire is a native closure/presence
check, not a broad `XDG_RUNTIME_DIR` passthrough. Flatpak build provenance
records actual OSTree commits but does not permanently pin a user's runtime.

See [docs/GSTREAMER-RUNTIME.md](../docs/GSTREAMER-RUNTIME.md) for official
upstream links and the copyright/patent/legal-review boundary.
