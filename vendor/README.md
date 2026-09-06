# Locked GStreamer runtime

Lumina's Linux `vendored-runtime` bundle is built from the exact lock in
[`gstreamer-1.0.lock.json`](gstreamer-1.0.lock.json). The approved #18 input
is upstream Cerbero 1.28.6's `gstreamer-1.0` meta package plus
`gstreamer-1.0-libav`; it is not a hand-copied Ubuntu package tree.

GStreamer publishes Linux source tarballs and uses Cerbero for deployment
packages; it does not publish a standalone Linux binary. The workflow therefore
uses the digest-pinned Ubuntu 24.04/glibc 2.39 builder recorded in the lock.
The glibc floor is Lumina's deployment policy, not an upstream GStreamer
guarantee.

## Discovery and build

Discovery is an intentional manual operation and is the only code allowed to
read moving upstream metadata:

```bash
./scripts/discover-gstreamer-lock.sh
```

The formal build reads only the lock, verifies its checksums, fetches the
locked sources and Cerbero archive, then packages offline:

```bash
./scripts/build-gstreamer-runtime.sh \
  --lock vendor/gstreamer-1.0.lock.json \
  --output dist/gstreamer-runtime
```

The lock also pins the Cerbero `DistTarball` flat archive layout. The
meta package may contain only the top-level roots `bin`, `etc`, `lib`,
`libexec`, and `share`; the libav package may contain only `lib`. The package
archives use Debian's native `lib/x86_64-linux-gnu` directory, and the
standalone runtime preserves the entire native `lib/` tree. The
`lib/x86_64-linux-gnu` and `lib/python3.12` directories remain siblings, so
Python purelib stays at `lib/python3.12/site-packages`; no path components are
stripped, and `/opt` roots are rejected. After assembling these packages and
the optional demo, the build recursively resolves every ELF `DT_NEEDED` entry
from the same Cerbero SDK and Ubuntu builder. Added libraries and their hashes
are recorded in `elf-dependencies.json`. glibc and hardware-specific GPU drivers
remain host-owned; generic graphics loaders are bundled. Missing dependencies
fail the build.

Vulkan's loader and the GIO TLS/proxy modules are included explicitly because
they are loaded dynamically. The launcher selects the private GIO modules and
the builder's bundled CA trust store without disabling certificate validation.
The isolated smoke also checks Vulkan loading and GIO TLS initialization.

Cerbero 1.28.6 routes one required dependency, zlib 1.3.1, through its
recipe URL rather than the GStreamer mirror. Discovery reads that exact pinned
recipe and records its checksum while constructing only the official GStreamer
mirror URL; the formal build verifies the recipe again and pre-seeds
`$XDG_CACHE_HOME/cerbero-sources/zlib-1.3.1/zlib-1.3.1.tar.gz` before the
dependency-aware Cerbero fetch. FFmpeg is built from the shared `ffmpeg.lock.json`. WebRTC audio processing
uses the canonical `www.freedesktop.org` source with the checksum from Cerbero;
the bare hostname rejects CI downloads.

The generated standalone artifact has this runtime layout:

```text
vendor/linux-x86_64/
├── bin/
│   └── lumina-gstreamer-runtime
├── lib/
│   ├── x86_64-linux-gnu/
│   │   └── gstreamer-1.0/
│   └── python3.12/
│       └── site-packages/
└── libexec/gstreamer-1.0/gst-plugin-scanner
```

Run the bundled launcher as the process entrypoint:

```bash
vendor/linux-x86_64/bin/lumina-gstreamer-runtime /path/to/lumina-video [args]
```

The launcher derives paths from its own runtime root, sets the exact private
library/plugin/scanner contract, clears unversioned and system plugin paths,
and creates a versioned registry under external `XDG_CACHE_HOME` (or
`$HOME/.cache`). It refuses a missing or unwritable cache and never writes the
bundle. The Rust seam only validates that launcher-established contract; a
missing or incomplete bundle is a decoder initialization error and never falls
back to host plugins.

## Scope boundary

This release deliberately accepts Cerbero's upstream meta-package closure as
the reproducibility/bootstrap boundary. Recursive closure pruning, ugly/GPL
classification, license/source inventory, and any compliance claim beyond the
upstream package metadata are deferred to issue #19. The lock pins the source
archives, Cerbero revision, OCI image, packages, and components; it does not
promise byte-identical artifacts across toolchains or filesystems.

The smoke script checks GStreamer 1.28.6, the locked MP4 elements, private
registry/plugin/scanner paths, and a deterministic H.264/AAC MP4 reaching EOS:

```bash
./fixtures/generate.sh
./scripts/smoke-gstreamer-runtime.sh \
  dist/gstreamer-runtime/gstreamer-runtime-linux-x86_64.tar.gz
```

The container receives the smoke script through stdin; playback must reach EOS
within 60 seconds. The driver regression check needs Python 3, Bash and jq,
but does not build a runtime or require Docker:

```bash
python3 scripts/test-smoke-gstreamer-runtime.py
```

## Release readiness

`package-gstreamer.yml` and `release-linux.yml` compile the demo against the
same Cerbero SDK, using `--build-demo`, and include it with the runtime. Extract
the artifact and run `./lumina-video-demo`; its launcher selects the bundled
libraries before the executable loads. The build uses Ubuntu 24.04/glibc 2.39.
The release workflow publishes this standalone archive rather than system
packages that require an unavailable distribution GStreamer version.

The checked-in Flatpak/AUR templates are not used by this release workflow.

Before claiming production artifacts, #19 still requires validation of dynamically
loaded playback and audio components, GPL/ugly exclusion, component versions
and checksums with licenses, full license texts and corresponding source/patch
archives, plus isolated standalone and Flatpak playback of the MP4, Matroska,
HLS VOD/live, audio and track fixtures. The #18 MP4 smoke and its driver regression
check do not establish any of those wider guarantees.
