// swift-tools-version: 6.0
import Foundation
import PackageDescription

// macOS Foundation/Security build of the native iOS core plus the generated UniFFI smoke.
// It is not an iOS build: the SwiftUI app target needs full Xcode and an iOS SDK.
let rustLibraryDirectory = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("../../target/debug")
    .standardizedFileURL.path

let package = Package(
    name: "OpenPushMobile",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "OpenPushMobileSmoke", targets: ["OpenPushMobileSmoke"])],
    targets: [
        .systemLibrary(name: "openpush_mobile_bindingsFFI", path: "Generated"),
        // The generator-owned Swift bindings, compiled once and shared by every target.
        .target(
            name: "OpenPushBindings",
            dependencies: ["openpush_mobile_bindingsFFI"],
            path: "Generated",
            exclude: [
                "openpush_mobile_bindingsFFI.h",
                "openpush_mobile_bindingsFFI.modulemap",
                "module.modulemap",
            ],
            sources: ["openpush_mobile_bindings.swift"],
            linkerSettings: [
                .unsafeFlags([
                    "-L\(rustLibraryDirectory)",
                    "-Xlinker", "-rpath", "-Xlinker", rustLibraryDirectory,
                ]),
                .linkedLibrary("openpush_mobile_bindings"),
            ]
        ),
        // Foundation/Security native core shared with the Xcode app target.
        .target(name: "OpenPushNative", dependencies: ["OpenPushBindings"], path: "OpenPushNative"),
        .executableTarget(name: "OpenPushMobileSmoke", dependencies: ["OpenPushBindings"], path: "Smoke"),
        .testTarget(
            name: "OpenPushNativeTests",
            dependencies: ["OpenPushNative", "OpenPushBindings"],
            path: "Tests/OpenPushNativeTests"
        ),
    ]
)
