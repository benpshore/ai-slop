// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "text_processing_engine",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "ShareIntake", targets: ["ShareIntake"]),
        .library(name: "ShareExtensionSupport", targets: ["ShareExtensionSupport"]),
    ],
    targets: [
        .target(name: "text_processing_engine"),
        .target(name: "ShareIntake"),
        .target(name: "ShareExtensionSupport", dependencies: ["ShareIntake"]),
        .testTarget(
            name: "text_processing_engineTests",
            dependencies: ["text_processing_engine", "ShareIntake"],
            path: "SwiftTests" // tests/ is Python's; macOS disks ignore case
        )
    ]
)
