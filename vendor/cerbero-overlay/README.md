# Lumina audited Cerbero overlay

This small, repository-owned overlay is copied into the pinned Cerbero tree by
`scripts/build-gstreamer-runtime.sh`. It is configuration, not a second build
system: the upstream `gstreamer-1.0` and `gstreamer-1.0-libav` recipes remain
the only package closure.

The lock is the authority for the exact variants and plugin set. The overlay
keeps Rust off, enables ALSA/Pulse/VA integration, and documents the LGPL-only
FFmpeg 7.1 configuration used for `avdec_h264` and `avdec_aac`. The builder
also checks the pinned Cerbero FFmpeg recipe's LGPL license and disabled
`nonfree`/`version3` Meson options before it fetches the closure. GPL, ugly,
and x264 features are rejected by the build audit.

References: the [Cerbero build guide](https://gstreamer.freedesktop.org/documentation/installing/building-from-source-using-cerbero.html)
and [FFmpeg licensing](https://ffmpeg.org/legal.html).
