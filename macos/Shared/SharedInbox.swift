import AppKit
import Foundation
import UniformTypeIdentifiers

public enum TPEMacConfiguration {
    public static let appGroup = "group.org.textprocessingengine.shared"
    public static let bookmarkDefaultsKey = "securityScopedPDFBookmarks"
}

public struct IntakeRecord: Codable, Identifiable, Sendable, Equatable {
    public let id: UUID
    public let filename: String
    public let receivedAt: Date
    public let bookmark: Data?

    public init(id: UUID = UUID(), filename: String, receivedAt: Date = Date(), bookmark: Data?) {
        self.id = id
        self.filename = filename
        self.receivedAt = receivedAt
        self.bookmark = bookmark
    }
}

/// The sole App Group communication boundary used by the host and extension.
/// Writes use an atomic property-list replacement so the two processes never
/// observe a partially-written queue.
public struct SharedInbox: Sendable {
    public let directory: URL

    public init(directory: URL? = nil) throws {
        if let directory {
            self.directory = directory
        } else if let group = FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: TPEMacConfiguration.appGroup
        ) {
            self.directory = group.appendingPathComponent("Inbox", isDirectory: true)
        } else {
            throw CocoaError(.fileNoSuchFile)
        }
        try FileManager.default.createDirectory(at: self.directory, withIntermediateDirectories: true)
    }

    public func importPDF(at source: URL) throws -> IntakeRecord {
        guard (try? source.resourceValues(forKeys: [.contentTypeKey]).contentType)?.conforms(to: .pdf) == true else {
            throw CocoaError(.fileReadUnsupportedScheme)
        }
        let access = source.startAccessingSecurityScopedResource()
        defer { if access { source.stopAccessingSecurityScopedResource() } }

        let bookmark = try? source.bookmarkData(
            options: .withSecurityScope,
            includingResourceValuesForKeys: [.contentTypeKey],
            relativeTo: nil
        )
        let record = IntakeRecord(filename: source.lastPathComponent, bookmark: bookmark)
        let destination = directory.appendingPathComponent("\(record.id.uuidString).pdf")
        try FileManager.default.copyItem(at: source, to: destination)
        let metadata = directory.appendingPathComponent("\(record.id.uuidString).plist")
        try PropertyListEncoder().encode(record).write(to: metadata, options: .atomic)
        return record
    }

    public func records() throws -> [IntakeRecord] {
        try FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: nil
        )
        .filter { $0.pathExtension == "plist" }
        .compactMap { try? PropertyListDecoder().decode(IntakeRecord.self, from: Data(contentsOf: $0)) }
        .sorted { $0.receivedAt > $1.receivedAt }
    }
}

public enum ShareImporter {
    /// Imports the first PDF attachment. Kept outside the view controller so
    /// XCTest can exercise the same NSItemProvider path as the extension.
    @MainActor
    public static func importFirstPDF(from providers: [NSItemProvider], into inbox: SharedInbox) async throws -> IntakeRecord {
        guard let provider = providers.first(where: { $0.hasItemConformingToTypeIdentifier(UTType.pdf.identifier) }) else {
            throw CocoaError(.fileReadUnsupportedScheme)
        }
        let item = try await provider.loadItem(forTypeIdentifier: UTType.pdf.identifier)
        if let url = item as? URL { return try inbox.importPDF(at: url) }
        if let data = item as? Data {
            let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString).appendingPathExtension("pdf")
            try data.write(to: temporary, options: .atomic)
            defer { try? FileManager.default.removeItem(at: temporary) }
            return try inbox.importPDF(at: temporary)
        }
        throw CocoaError(.fileReadCorruptFile)
    }
}
