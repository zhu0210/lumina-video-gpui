# Lumina GStreamer runtime research

Snapshot: 2026-08-14. This note records the primary upstream facts used by
`vendor/gstreamer-1.0.lock.json`; the lock is the build authority.

## Upstream facts

| Decision | Primary source | Recorded result |
| --- | --- | --- |
| Linux installation/build route | [GStreamer download](https://gstreamer.freedesktop.org/download/) and [Cerbero build guide](https://gstreamer.freedesktop.org/documentation/installing/building-from-source-using-cerbero.html) | GStreamer documents package-manager installation for Linux and Cerbero for building/deploying releases. The official download page does not provide a standalone Linux runtime binary for this use case, so Lumina builds one with Cerbero. |
| Exact GStreamer source | [official source index](https://gstreamer.freedesktop.org/src/gstreamer/) and [1.28.6 checksum sidecar](https://gstreamer.freedesktop.org/src/gstreamer/gstreamer-1.28.6.tar.xz.sha256sum) | `gstreamer-1.28.6.tar.xz`, SHA-256 `62b6b9f0ad3147a6dd6420ac64a91180b14e990695bddd353b96041611d052ca`. |
| Exact libav source | [official gst-libav source index](https://gstreamer.freedesktop.org/src/gst-libav/) and [1.28.6 checksum sidecar](https://gstreamer.freedesktop.org/src/gst-libav/gst-libav-1.28.6.tar.xz.sha256sum) | `gst-libav-1.28.6.tar.xz`, SHA-256 `71e6eafb4fff2a66d1bb0ba8d078224dfe7e3397307d8c0bba3dc23606e08f51`. |
| Cerbero release selection | [Cerbero GitLab repository](https://gitlab.freedesktop.org/gstreamer/cerbero), [tag API](https://gitlab.freedesktop.org/api/v4/projects/gstreamer%2Fcerbero/repository/tags/1.28.6), [locked archive](https://gitlab.freedesktop.org/gstreamer/cerbero/-/archive/1.28.6/cerbero-1.28.6.tar.gz) | Annotated tag object `78666745b34b6245a85510ac47a03a5033af4711`, peeled commit `59548269f4fd0f701818f0bafdb102959ec81e65`, archive SHA-256 `16cea2f8c34370f7f1e5774c4c4f66927659c354f6f90ce0e13d41cf80cc34da`. |
| Cerbero package/artifact model | [Cerbero build guide](https://gstreamer.freedesktop.org/documentation/installing/building-from-source-using-cerbero.html) | Cerbero documents `package gstreamer-1.0`, tarball artifacts from 1.28 onward, variants, and fetch/bootstrap/package offline staging. #18 uses only the approved `gstreamer-1.0` and `gstreamer-1.0-libav` packages with `norust`. |
| Runtime isolation | [GStreamer environment variables](https://gstreamer.freedesktop.org/documentation/gstreamer/running.html) | The versioned `GST_PLUGIN_PATH_1_0`, empty `GST_PLUGIN_SYSTEM_PATH_1_0`, and `GST_REGISTRY_1_0` variables are the supported private-plugin/registry controls. GStreamer’s 1.28 test setup also records `GST_PLUGIN_SCANNER_1_0`; Lumina sets it to the bundle scanner. |
| Builder identity | [Docker Registry API](https://docs.docker.com/reference/api/registry/latest/) and [Ubuntu official image metadata](https://raw.githubusercontent.com/docker-library/official-images/master/library/ubuntu) | The workflow uses the immutable Ubuntu 24.04 amd64 OCI manifest `ubuntu@sha256:019e8eb29a85e74d64925745884f2ec79aa27e3feab36353d24656f4d6b89467`. glibc 2.39 is verified in the job and is Lumina’s deployment policy. |

The required MP4 smoke elements are derived from the locked Cerbero 1.28.6
package recipes and are recorded as filename-to-element pairs in the lock:
`playbin3`, `qtdemux`, `h264parse`, `avdec_h264`, `aacparse`, `avdec_aac`, and
`fakesink`. The smoke checks both the plugin filename and GStreamer’s reported
`Filename` before running `playbin3` to EOS.

## Reproducibility boundary

`scripts/discover-gstreamer-lock.sh` is the only moving-metadata path. It reads
the official 1.28.x download/source metadata, the official Cerbero tag API and
Git references, and the Docker registry manifest, computes archive checksums,
then atomically writes the lock. `scripts/build-gstreamer-runtime.sh` rejects
moving `latest` values, reads only that lock, verifies the two source checksums,
fetches the remaining Cerbero recipe closure, and runs the package phase
offline.

The lock's `artifact.compression: xz` describes the two Cerbero package
tarballs; the build combines them and emits the standalone runtime as a
`.tar.gz` artifact while preserving Cerbero's native `lib/` tree.
`lib/x86_64-linux-gnu` and `lib/python3.12` remain siblings, so Python purelib
stays at `lib/python3.12/site-packages`; the path is kept as-is.

The artifact includes `bin/lumina-gstreamer-runtime`. Applications must use
that launcher as their entrypoint: it establishes exact private
`LD_LIBRARY_PATH`, versioned GStreamer plugin/scanner paths, empty system and
unversioned plugin variables, and a writable registry/cache outside the bundle.
The Rust vendored-runtime seam validates this contract and reports
`DecoderInit` when it is absent or mismatched; it does not mutate process-wide
environment state. Final Lumina executable packaging/integration is deferred
to issue #19.

The lock fixes the source archives, Cerbero revision, OCI image, package set,
variants, and required components. Those locked inputs make the build
inputs reproducible, but do not promise byte-identical output across
toolchains, filesystems, or archive implementations.

This is intentionally an upstream meta-package bootstrap, not a claim that the
result is a recursively pruned or fully inventoried distribution. Recursive
closure pruning, ugly/GPL classification, complete license/source inventory,
and the associated compliance review are deferred to issue #19.
