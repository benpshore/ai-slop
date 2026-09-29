// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "text_processing_engine",
    platforms: [.macOS(.v15)],
    targets: [
        .target(name: "text_processing_engine"),
        .testTarget(
            name: "text_processing_engineTests",
            dependencies: ["text_processing_engine"]
            , path: "SwiftTests"  // tests/ is Python's; macOS disks ignore case
        )
    ]
)
