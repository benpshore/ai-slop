import Foundation

/// The UI's adapter boundary, not a replacement for the Rust engine's API schema.
/// A future adapter must preserve partial outcomes and verified identifier evidence.
public protocol EngineClient: Sendable {
    func run(
        _ request: ExtractionRequest,
        progress: @escaping @Sendable (JobProgress) async -> Void
    ) async throws -> ExtractionResult
}

public enum ExtractionMode: String, CaseIterable, Codable, Sendable, Identifiable {
    case text, bibliography, both
    public var id: Self { self }
    public var title: String {
        switch self {
        case .text: "Text"
        case .bibliography: "Bibliography"
        case .both: "Text and bibliography"
        }
    }
}

public enum EnginePreference: String, CaseIterable, Codable, Sendable, Identifiable {
    case automatic, lopdf, pdfium, docling
    public var id: Self { self }
    public var title: String {
        switch self {
        case .automatic: "Automatic"
        case .lopdf: "lopdf"
        case .pdfium: "PDFium"
        case .docling: "Docling"
        }
    }
}

public struct ExtractionOptions: Codable, Equatable, Sendable {
    public var mode: ExtractionMode
    public var engine: EnginePreference
    public var preserveLineBreaks: Bool

    public init(
        mode: ExtractionMode = .both,
        engine: EnginePreference = .automatic,
        preserveLineBreaks: Bool = true
    ) {
        self.mode = mode
        self.engine = engine
        self.preserveLineBreaks = preserveLineBreaks
    }
}

public enum PreviewScenario: String, CaseIterable, Codable, Sendable, Identifiable {
    case complete, limited, failure
    public var id: Self { self }
    public var title: String {
        switch self {
        case .complete: "Complete sample"
        case .limited: "Limited sample"
        case .failure: "Failed sample"
        }
    }
}

public struct DocumentSource: Equatable, Sendable {
    public let name: String
    public let fileURL: URL?
    public let previewScenario: PreviewScenario
    public let identity: String

    public init(fileURL: URL) {
        let canonical = fileURL.standardizedFileURL.resolvingSymlinksInPath()
        self.name = canonical.lastPathComponent
        self.fileURL = canonical
        self.previewScenario = .complete
        self.identity = canonical.path
    }

    public init(sample: PreviewScenario) {
        self.name = switch sample {
        case .complete: "Research methods — sample.pdf"
        case .limited: "Complex figures — sample.pdf"
        case .failure: "Unreadable document — sample.pdf"
        }
        self.fileURL = nil
        self.previewScenario = sample
        self.identity = "sample:\(sample.rawValue)"
    }
}

public struct ExtractionRequest: Sendable {
    public let id: UUID
    public let source: DocumentSource
    public let options: ExtractionOptions

    public init(id: UUID, source: DocumentSource, options: ExtractionOptions) {
        self.id = id
        self.source = source
        self.options = options
    }
}

public struct JobProgress: Equatable, Sendable {
    public let completedPages: Int
    public let totalPages: Int?
    public let message: String

    public init(completedPages: Int, totalPages: Int?, message: String) {
        self.completedPages = max(0, completedPages)
        self.totalPages = totalPages.flatMap { $0 > 0 ? $0 : nil }
        self.message = message
    }

    public var fraction: Double? {
        totalPages.map { min(1, Double(completedPages) / Double($0)) }
    }
}

public enum ExtractionCompleteness: String, Codable, Sendable {
    case complete, limited
}

public struct ReferenceEntry: Codable, Equatable, Sendable, Identifiable {
    public let id: Int
    public let authors: String
    public let title: String
    public let publication: String
    public let identifier: String?
    public let identifierVerification: String

    public init(
        id: Int, authors: String, title: String, publication: String,
        identifier: String?, identifierVerification: String
    ) {
        self.id = id
        self.authors = authors
        self.title = title
        self.publication = publication
        self.identifier = identifier
        self.identifierVerification = identifierVerification
    }
}

public struct ExtractionResult: Codable, Equatable, Sendable {
    public let completeness: ExtractionCompleteness
    public let isSimulated: Bool
    public let backendIdentity: String
    public let pagesProcessed: Int
    public let text: String?
    public let references: [ReferenceEntry]?
    public let warnings: [String]

    public init(
        completeness: ExtractionCompleteness, isSimulated: Bool, backendIdentity: String,
        pagesProcessed: Int, text: String?, references: [ReferenceEntry]?, warnings: [String]
    ) {
        self.completeness = completeness
        self.isSimulated = isSimulated
        self.backendIdentity = backendIdentity
        self.pagesProcessed = pagesProcessed
        self.text = text
        self.references = references
        self.warnings = warnings
    }
}

public enum PreviewError: LocalizedError {
    case unreadableSample

    public var errorDescription: String? {
        "Simulated failure: this sample represents a PDF the engine could not open. No file was processed."
    }
}

/// Produces fixtures only. It never opens the input, launches a process or makes a network call.
public struct PreviewEngineClient: EngineClient {
    public let stepDelay: Duration

    public init(stepDelay: Duration = .milliseconds(240)) {
        self.stepDelay = stepDelay
    }

    public func run(
        _ request: ExtractionRequest,
        progress: @escaping @Sendable (JobProgress) async -> Void
    ) async throws -> ExtractionResult {
        for page in 0...4 {
            try Task.checkCancellation()
            await progress(JobProgress(
                completedPages: page, totalPages: 4,
                message: page == 0 ? "Preparing sample" : "Simulating page \(page) of 4"
            ))
            if stepDelay > .zero { try await Task.sleep(for: stepDelay) }
        }
        try Task.checkCancellation()
        if request.source.previewScenario == .failure { throw PreviewError.unreadableSample }
        return Self.result(for: request)
    }

    public static func result(for request: ExtractionRequest) -> ExtractionResult {
        let limited = request.source.previewScenario == .limited
        let paragraphs = [
            "SAMPLE OUTPUT · NOT EXTRACTED FROM YOUR PDF",
            "Research methods for dependable document processing",
            "Abstract",
            "A document workflow should make its evidence easy to inspect. This preview shows how extracted text, reference entries and incomplete outcomes will appear when an engine adapter is connected.",
            "1. Methods",
            "Source documents remain separate from derived results. A completed job records the backend used, pages processed and any warnings. A limited extraction remains visibly incomplete.",
            "2. Findings",
            "The examples in this workspace are fictional. They do not measure extraction accuracy, verify identifiers or describe the contents of an imported PDF.",
        ]
        let text = paragraphs.joined(separator: request.options.preserveLineBreaks ? "\n\n" : " ")
        let references = [
            ReferenceEntry(
                id: 1, authors: "Morgan A, Chen B", title: "Inspecting document evidence",
                publication: "Example Journal · 2026 · fictional sample", identifier: nil,
                identifierVerification: "Not measured — sample data"
            ),
            ReferenceEntry(
                id: 2, authors: "Rivera C, Patel D", title: "Keeping incomplete outcomes visible",
                publication: "Example Proceedings · 2026 · fictional sample", identifier: nil,
                identifierVerification: "Not measured — sample data"
            ),
        ]
        return ExtractionResult(
            completeness: limited ? .limited : .complete,
            isSimulated: true, backendIdentity: "offline-preview/1",
            pagesProcessed: limited ? 3 : 4,
            text: request.options.mode == .bibliography ? nil : text,
            references: request.options.mode == .text ? nil : references,
            warnings: limited
                ? ["Sample resource limit reached on page 4. The result is incomplete."] : []
        )
    }
}
