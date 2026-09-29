import AppKit
import UniformTypeIdentifiers
import XCTest
@testable import TPEMacSupport

final class SharedInboxTests: XCTestCase {
    private func fixture(in directory: URL) throws -> URL {
        let url = directory.appendingPathComponent("paper.pdf")
        try Data("%PDF-1.4\n%%EOF\n".utf8).write(to: url)
        return url
    }

    func testFileOpenAndDropUseAtomicSharedInbox() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let inbox = try SharedInbox(directory: root.appendingPathComponent("Inbox"))
        let record = try inbox.importPDF(at: try fixture(in: root))
        XCTAssertEqual(record.filename, "paper.pdf")
        XCTAssertEqual(try inbox.records(), [record])
    }

    @MainActor
    func testShareProviderImportsPDF() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let inbox = try SharedInbox(directory: root.appendingPathComponent("Inbox"))
        let provider = NSItemProvider(contentsOf: try fixture(in: root))!
        let record = try await ShareImporter.importFirstPDF(from: [provider], into: inbox)
        XCTAssertEqual(record.filename, "paper.pdf")
    }
}
