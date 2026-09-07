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
  const moq = await import("@moq/net");
  assert.equal(typeof moq.Connection.connect, "function");
  assert.equal(moq.Path.from("live/video"), "live/video");
});


test("MoQ bridge reads current structured catalog frames and cleans failed connections", async () => {
  const moq = await import("@moq/net");
  const producer = new moq.Broadcast.Producer();
  const catalog = producer.createTrack("catalog.json");
  const broadcast = producer.consume();
  const subscribe = broadcast.subscribe.bind(broadcast);
  broadcast.subscribe = (name, options) => {
    assert.deepEqual(options, { priority: name === "catalog.json" ? 100 : 60 });
    return subscribe(name, options);
  };
  let closed = false;
  globalThis.window = { MoqNet: { ...moq, Connection: {
    connect: async (_url, options) => {
      assert.ok(options.signal instanceof AbortSignal);
      return { version: "moq-lite", closed: new Promise(() => {}),
        consume: () => broadcast, close: () => { closed = true; } };
    },
  } } };
  const bridge = await import("./moq-transport-bridge.js");
  const id = await bridge.moqConnect("https://relay.example", "live/video");
  const group = catalog.appendGroup();
  group.writeJson({ video: { renditions: { main: { codec: "avc1.64001f" } } } });
  group.close();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(JSON.parse(bridge.moqGetCatalog(id)).video[0].codec, "avc1.64001f");
  assert.equal(bridge.moqGetExtendedStats(id).connectionVersion, "moq-lite");
  const chunks = [];
  let decoderClosed = false;
  globalThis.VideoDecoder = class {
    state = "unconfigured";
    decodeQueueSize = 0;
    configure(config) { if (!config.codec) throw new Error("invalid codec"); this.state = "configured"; }
    decode(chunk) { chunks.push(chunk); }
    close() { decoderClosed = true; this.state = "closed"; }
  };
  globalThis.EncodedVideoChunk = class { constructor(options) { Object.assign(this, options); } };
  const video = producer.createTrack("video");
  bridge.moqStartVideo(id, "video", "avc1.64001f", 64, 64, "legacy", 1, null, () => {}, () => {});
  const videoGroup = video.appendGroup();
  videoGroup.writeFrame({ timestamp: moq.Time.Timestamp.fromMicros(5), payload: new Uint8Array([5, 0xaa]) });
  videoGroup.close();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(chunks.length, 1);
  assert.equal(chunks[0].timestamp, 5);
  assert.deepEqual(chunks[0].data, new Uint8Array([0xaa]));
  assert.throws(() => bridge.moqStartVideo(id, "video", "", 64, 64, "legacy", 1, null, () => {}, () => {}), /invalid codec/);
  assert.equal(window.__moqTransportBridge.activeVideoDecoders.size, 1);
  bridge.moqDisconnect(id);
  assert.equal(closed, true);
  assert.equal(decoderClosed, true);
  assert.equal(window.__moqTransportBridge.activeVideoDecoders.size, 0);
  assert.equal(window.__moqTransportBridge.activeSessions.size, 0);
  window.MoqNet.Connection.connect = async () => { throw new Error("failed connection"); };
  await assert.rejects(bridge.moqConnect("https://relay.example", "live/video"), /failed connection/);
  assert.equal(window.__moqTransportBridge.activeSessions.size, 0);
  producer.close();
});

test("video exposes failed play and media errors, but ignores a play cancelled by pause", async () => {
  const { playVideo, pauseVideo, getVideoError } = await import("./video-bridge.js");
  let reject;
  const video = { play: () => new Promise((_, fail) => { reject = fail; }), pause() {} };
  playVideo(video);
  reject(new Error("NotAllowedError"));
  await Promise.resolve();
  assert.match(getVideoError(video), /NotAllowedError/);
  playVideo(video);
  assert.equal(getVideoError(video), null);
  pauseVideo(video);
  reject(new Error("AbortError"));
  await Promise.resolve();
  assert.equal(getVideoError(video), null);
  video.error = { code: 4, message: "Unsupported codec" };
  assert.match(getVideoError(video), /Unsupported codec/);
});

test("HLS manifest preserves pause and exhausted recovery becomes a player error", async () => {
  const { initHls, getVideoError } = await import("./video-bridge.js");
  const previous = globalThis.Hls;
  class FakeHls {
    static Events = { MANIFEST_PARSED: "manifest", LEVEL_SWITCHED: "level", ERROR: "error", FRAG_LOADED: "fragment" };
    static ErrorTypes = { NETWORK_ERROR: "network", MEDIA_ERROR: "media" };
    static isSupported() { return true; }
    handlers = new Map();
    retries = 0;
    destroyed = false;
    on(event, handler) { this.handlers.set(event, handler); }
    loadSource() {}
    attachMedia() {}
    startLoad() { this.retries++; }
    recoverMediaError() { this.retries++; }
    destroy() { this.destroyed = true; }
  }
  globalThis.Hls = FakeHls;
  try {
    const video = { play() { assert.fail("manifest must not initiate playback"); } };
    const hls = initHls(video, "https://example.test/video.m3u8");
    hls.handlers.get("manifest")(null, { levels: [] });
    const error = { fatal: true, type: "network", details: "manifestLoadError" };
    for (let i = 0; i < 3; i++) hls.handlers.get("error")(null, error);
    assert.equal(getVideoError(video), null);
    hls.handlers.get("error")(null, error);
    assert.equal(hls.retries, 3);
    assert.equal(hls.destroyed, true);
    assert.match(getVideoError(video), /manifestLoadError/);
  } finally {
    globalThis.Hls = previous;
  }
});
