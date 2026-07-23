# Contributing

This repository is a managed GPUI-focused fork. Changes should preserve the
three crate boundaries described in the README and keep native decoding details
out of the GPUI layer.

## Before submitting changes

Format and validate the crates you touched:

```sh
cargo fmt --all -- --check
cargo clippy -p lumina-video-core -p lumina-video-wgpu -p lumina-video -- -D warnings
cargo test -p lumina-video-core --lib
cargo test -p lumina-video-wgpu --lib
```

Changes affecting browser code must also pass:

```sh
cargo check -p lumina-video --target wasm32-unknown-unknown
cargo clippy -p lumina-video --target wasm32-unknown-unknown -- -D warnings
```

Platform-specific import work must validate handle ownership, dimensions,
strides, offsets, allocation bounds, modifiers, synchronization, and partial
failure cleanup. Do not add render-thread waits or process-global decoder
selection.

Keep commits separated by concern: dependency synchronization, API cleanup,
renderer optimization, platform interop, and GPUI integration.
