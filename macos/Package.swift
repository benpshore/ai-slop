// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "TPEMacSupport",
    platforms: [.macOS(.v15)],
    products: [.library(name: "TPEMacSupport", targets: ["TPEMacSupport"])],
    targets: [
        .target(name: "TPEMacSupport", path: "Shared"),
        .testTarget(name: "TPEMacSupportTests", dependencies: ["TPEMacSupport"], path: "Tests"),
    ]
)
