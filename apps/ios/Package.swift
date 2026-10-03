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
    name: "PeppyMobile",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "PeppyMobileSmoke", targets: ["PeppyMobileSmoke"])],
    targets: [
        .systemLibrary(name: "peppy_mobile_bindingsFFI", path: "Generated"),
        // The generator-owned Swift bindings, compiled once and shared by every target.
        .target(
            name: "PeppyBindings",
            dependencies: ["peppy_mobile_bindingsFFI"],
            path: "Generated",
            exclude: [
                "peppy_mobile_bindingsFFI.h",
                "peppy_mobile_bindingsFFI.modulemap",
                "module.modulemap",
            ],
            sources: ["peppy_mobile_bindings.swift"],
            linkerSettings: [
                .unsafeFlags([
                    "-L\(rustLibraryDirectory)",
                    "-Xlinker", "-rpath", "-Xlinker", rustLibraryDirectory,
                ]),
                .linkedLibrary("peppy_mobile_bindings"),
            ]
        ),
        // Objective-C wrapper for the Contacts change-history API, which Swift cannot call directly.
        .target(
            name: "PeppyContactsHistory",
            path: "ContactsHistory",
            publicHeadersPath: "include",
            linkerSettings: [.linkedFramework("Contacts")]
        ),
        // Foundation/Security native core shared with the Xcode app target.
        .target(name: "PeppyNative", dependencies: ["PeppyBindings", "PeppyContactsHistory"], path: "PeppyNative"),
        .executableTarget(name: "PeppyMobileSmoke", dependencies: ["PeppyBindings"], path: "Smoke"),
        .testTarget(
            name: "PeppyNativeTests",
            dependencies: ["PeppyNative", "PeppyBindings"],
            path: "Tests/PeppyNativeTests"
        ),
    ]
)
