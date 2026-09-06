import LuminaVideoBridge
import Metal
import UIKit
import XCTest
@testable import LuminaTestHarness

private final class FrameReceiver: LuminaVideoPlayerDelegate {
    var receive: (@MainActor (LuminaVideoFrame) -> Void)?

    func luminaPlayer(_ player: LuminaVideoPlayer, didChangeState state: LuminaVideoState) {}

    func luminaPlayer(_ player: LuminaVideoPlayer, didReceiveFrame frame: LuminaVideoFrame) {
        Task { @MainActor [weak self] in self?.receive?(frame) }
    }
}

final class PlaybackTests: XCTestCase {
    /// Exercises the Swift bridge and Metal harness, not the GPUI renderer.
    @MainActor
    func testNativeFramesPlaybackSeekAndDestruction() async throws {
        let url = try XCTUnwrap(Bundle(for: Self.self).url(forResource: "h264-aac", withExtension: "mp4"))
        _ = try XCTUnwrap(MTLCreateSystemDefaultDevice(), "Simulator must provide Metal")
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 180))
        let controller = UIViewController()
        let view = MetalVideoUIView(frame: window.bounds)
        controller.view = view
        window.rootViewController = controller
        window.makeKeyAndVisible()
        view.layoutIfNeeded()
        defer {
            view.currentFrame = nil
            view.onFrameCompleted = nil
            window.isHidden = true
            window.rootViewController = nil
        }

        var player: LuminaVideoPlayer? = try LuminaVideoPlayer(url: url.absoluteString)
        weak var weakPlayer = player
        let receiver = FrameReceiver()
        var received = 0
        var completed = 0
        var failedGPU = false
        var missingSurface = false
        receiver.receive = { frame in
            received += 1
            missingSurface = missingSurface || frame.ioSurface == nil
            XCTAssertGreaterThan(frame.width, 0)
            XCTAssertGreaterThan(frame.height, 0)
            view.currentFrame = frame
        }
        view.onFrameCompleted = { status in
            completed += 1
            failedGPU = failedGPU || status != .completed
        }
        player?.delegate = receiver
        player?.play()
        try await eventually("first native frame and GPU completion") { received > 0 && completed > 0 }
        XCTAssertFalse(missingSurface, "Missing IOSurface is a failure, not a simulator skip")
        try await eventually("playback advances") { (player?.current_time ?? 0) > 0.3 }

        player?.pause()
        try await eventually("pause becomes visible") { player?.state == .paused }
        let pausedTime = try XCTUnwrap(player?.current_time)
        try await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertEqual(try XCTUnwrap(player?.current_time), pausedTime, accuracy: 0.1)
        let beforeSeek = received
        player?.seek(to: 1.0)
        try await eventually("paused seek produces a frame") {
            received > beforeSeek && abs((player?.current_time ?? 0) - 1.0) < 0.25
        }
        XCTAssertEqual(player?.state, .paused)

        player?.play()
        try await eventually("end of stream") { player?.state == .ended }
        let beforeEOSSeek = received
        player?.seek(to: 0.25)
        try await eventually("seek after EOS resumes frame polling without play") {
            received > beforeEOSSeek && (player?.current_time ?? 2) < 0.75
        }
        XCTAssertFalse(missingSurface)
        XCTAssertFalse(failedGPU)

        // A submitted frame may outlive its player; its decoder lease must survive
        // until the last GPU completion, then be released with the view's reference.
        weak var lastFrame = view.currentFrame
        player?.delegate = nil
        player = nil
        XCTAssertNil(weakPlayer)
        receiver.receive = nil
        view.currentFrame = nil
        try await eventually("last frame is released after GPU completion") { lastFrame == nil }
    }

    @MainActor
    private func eventually(_ description: String, condition: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(15)
        while !condition() {
            if Date() >= deadline {
                XCTFail("Timed out waiting for \(description)")
                throw NSError(domain: "LuminaPlaybackTests", code: 1)
            }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
    }
}
