// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "LuminaVideoBridge",
    platforms: [.iOS(.v16)],
    products: [
        .library(name: "LuminaVideoBridge", targets: ["LuminaVideoBridge"]),
    ],
    targets: [
        .binaryTarget(
            name: "CLuminaVideo",
            path: "Artifacts/CLuminaVideo.xcframework"
        ),
        .target(
            name: "LuminaVideoBridge",
            dependencies: ["CLuminaVideo"],
            path: "Sources/LuminaVideoBridge",
            linkerSettings: [
                .linkedFramework("AVFoundation"),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("CoreVideo"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("Metal"),
                .linkedFramework("IOSurface"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("SystemConfiguration"),
                .linkedLibrary("z"),
                .linkedLibrary("iconv"),
                .linkedLibrary("bz2"),
                .linkedLibrary("c++"),
            ]
        ),
    ]
)
