import test from "node:test";
import assert from "node:assert/strict";
import { requestVideoFrameCallback, cancelVideoFrameCallback } from "./video-bridge.js";

test("video callback survives EOS/replay and cancels before Rust closure destruction", () => {
  let callback, cancelled, frames = 0;
  const video = { isConnected: true, ended: true, currentTime: 1,
    requestVideoFrameCallback(fn) { callback = fn; return 7; },
    cancelVideoFrameCallback(id) { cancelled = id; },
  };
  requestVideoFrameCallback(video, () => frames++);
  callback(0, {});
  video.ended = false;
  callback(1, {});
  assert.equal(frames, 2);
  const stale = callback;
  cancelVideoFrameCallback(video);
  stale(2, {});
  assert.equal(cancelled, 7);
  assert.equal(frames, 2);
});

test("published MoQ dependency exposes the transport API consumed by the bridge", async () => {
  const moq = await import("@moq/lite");
  assert.equal(typeof moq.Connection.connect, "function");
  assert.equal(moq.Path.from("live/video"), "live/video");
});
