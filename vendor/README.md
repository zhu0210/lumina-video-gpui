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

Cerbero 1.28.6 routes one required dependency, zlib 1.3.1, through its
recipe URL rather than the GStreamer mirror. Discovery reads that exact pinned
recipe and records its checksum while constructing only the official GStreamer
mirror URL; the formal build verifies the recipe again and pre-seeds
`$XDG_CACHE_HOME/cerbero-sources/zlib-1.3.1/zlib-1.3.1.tar.gz` before the
dependency-aware Cerbero fetch. This is one locked acquisition exception, not
the recursive closure/license/source inventory deferred to issue #19.

The generated standalone artifact has this runtime layout:

```text
vendor/linux-x86_64/
├── bin/
│   └── lumina-gstreamer-runtime
├── lib/
│   └── gstreamer-1.0/
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
back to host plugins. Final Lumina executable packaging/integration is deferred
to issue #19.

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
