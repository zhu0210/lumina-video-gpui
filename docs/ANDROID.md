# Android native decoder

Android decoder and frame-delivery functionality is maintained in
`lumina-video-native-frame`; the obsolete facade/JNI demo crate is no longer
part of the workspace.

Check the native MediaCodec path with:

```bash
rustup target add aarch64-linux-android
cargo check --package lumina-video-native-frame --target aarch64-linux-android
```

The decoder uses Android's MediaCodec APIs and can expose native frame memory
to the renderer. Platform integration belongs to the application embedding the
native-frame crate.
