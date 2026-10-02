// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "PDFTextractMacOS",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "PDFTextractPreview", targets: ["PDFTextractMacOS"]),
        .library(name: "WorkspaceCore", targets: ["WorkspaceCore"]),
    ],
    targets: [
        .target(name: "WorkspaceCore"),
        .executableTarget(name: "PDFTextractMacOS", dependencies: ["WorkspaceCore"]),
        .testTarget(name: "WorkspaceCoreTests", dependencies: ["WorkspaceCore"]),
    ]
)
