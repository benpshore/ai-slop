import Foundation

/// The durable hand-off contract between the Share extension and the host.
public struct IntakeRequestEnvelope: Codable, Equatable, Sendable {
    public static let currentVersion = 1

    public let version: Int
    public let requestIdentifier: UUID
    public let stagedFileURL: URL
    public let originalDisplayName: String
    public let sourceApplication: String?
    public let creationTime: Date
    public let processingOptions: [String: String]?

    public init(
        version: Int = currentVersion,
        requestIdentifier: UUID = UUID(),
        stagedFileURL: URL,
        originalDisplayName: String,
        sourceApplication: String? = nil,
        creationTime: Date = Date(),
        processingOptions: [String: String]? = nil
    ) {
        self.version = version
        self.requestIdentifier = requestIdentifier
        self.stagedFileURL = stagedFileURL
        self.originalDisplayName = originalDisplayName
        self.sourceApplication = sourceApplication
        self.creationTime = creationTime
        self.processingOptions = processingOptions
    }
}

public struct IntakeDirectories: Sendable {
    public let root: URL
    public var staged: URL { root.appendingPathComponent("Staged", isDirectory: true) }
    public var requests: URL { root.appendingPathComponent("Requests", isDirectory: true) }
    public var acknowledgements: URL {
        root.appendingPathComponent("Acknowledgements", isDirectory: true)
    }

    public init(appGroupIdentifier: String, fileManager: FileManager = .default) throws {
        #if os(macOS)
            guard let root = fileManager.containerURL(
                forSecurityApplicationGroupIdentifier: appGroupIdentifier
            ) else { throw IntakeError.appGroupUnavailable }
            self.root = root.appendingPathComponent("ShareIntake", isDirectory: true)
        #else
            throw IntakeError.appGroupUnavailable
        #endif
    }

    public init(root: URL) { self.root = root }

    public func create(fileManager: FileManager = .default) throws {
        for directory in [root, staged, requests, acknowledgements] {
            try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        }
    }
}

public enum IntakeError: Error, Equatable, LocalizedError, Sendable {
    case appGroupUnavailable
    case unsupportedInput(String)
    case inputTooLarge(limit: Int64)
    case providerFailed(String)
    case cancelled
    case invalidEnvelope

    public var errorDescription: String? {
        switch self {
        case .appGroupUnavailable: "The shared import container is unavailable."
        case .unsupportedInput(let name): "\(name) is not a supported PDF."
        case .inputTooLarge(let limit): "The PDF is larger than the \(limit)-byte import limit."
        case .providerFailed(let reason): "The sharing application could not provide the PDF: \(reason)"
        case .cancelled: "The import was cancelled."
        case .invalidEnvelope: "The shared import request is invalid."
        }
    }
}
