#!/usr/bin/env bash
# CI runs inside the official Arch container so headers, plugins and runtime
# come from one repository. Ubuntu LTS ships an older GStreamer ABI.
set -euo pipefail
pacman -S --noconfirm --needed \
  clang cmake pkgconf openssl libunwind alsa-lib speech-dispatcher \
  libxcb libxkbcommon libxkbcommon-x11 libx11 libxi libxrandr libxcursor \
  wayland fontconfig freetype2 dbus vulkan-icd-loader mesa \
  gstreamer gst-plugins-base gst-plugins-base-libs gst-plugins-good \
  gst-plugins-bad gst-libav ffmpeg
pkg-config --atleast-version=1.28 gstreamer-1.0
gst-inspect-1.0 --version
for plugin in decodebin3 uridecodebin3 h264parse aacparse avdec_h264 avdec_aac; do
  gst-inspect-1.0 "$plugin" >/dev/null
done
