import Foundation

public enum ProvidedPDF: Sendable {
    case file(URL, displayName: String?)
    case data(Data, displayName: String?)
}

/// An adapter kept deliberately smaller than NSItemProvider, so staging is testable.
public protocol PDFItemProviding: Sendable {
    var suggestedName: String? { get }
    func loadPDF() async throws -> ProvidedPDF
}

public protocol HostNotifying: Sendable {
    func notifyHost(requests: [UUID]) async
}

public struct NullHostNotifier: HostNotifying {
    public init() {}
    public func notifyHost(requests _: [UUID]) async {}
}

public actor IntakeStager {
    private let directories: IntakeDirectories
    private let maximumBytes: Int64
    private let fileManager: FileManager
    private let encoder: JSONEncoder

    public init(
        directories: IntakeDirectories,
        maximumBytes: Int64 = 250 * 1_024 * 1_024,
        fileManager: FileManager = .default
    ) throws {
        self.directories = directories
        self.maximumBytes = maximumBytes
        self.fileManager = fileManager
        self.encoder = JSONEncoder()
        try directories.create(fileManager: fileManager)
    }

    /// Stages every item before notifying the host. Extraction never occurs here.
    public func stage(
        providers: [any PDFItemProviding],
        sourceApplication: String? = nil,
        processingOptions: [String: String]? = nil,
        notifier: any HostNotifying = NullHostNotifier()
    ) async throws -> [IntakeRequestEnvelope] {
        guard !providers.isEmpty else { throw IntakeError.unsupportedInput("The shared item") }
        var completed: [IntakeRequestEnvelope] = []
        do {
            for provider in providers {
                try Task.checkCancellation()
                let provided: ProvidedPDF
                do { provided = try await provider.loadPDF() } catch is CancellationError {
                    throw IntakeError.cancelled
                } catch let error as IntakeError {
                    throw error
                } catch {
                    throw IntakeError.providerFailed(error.localizedDescription)
                }
                let envelope = try persist(
                    provided,
                    fallbackName: provider.suggestedName,
                    sourceApplication: sourceApplication,
                    processingOptions: processingOptions
                )
                completed.append(envelope)
            }
            await notifier.notifyHost(requests: completed.map(\.requestIdentifier))
            return completed
        } catch {
            // A batch is all-or-nothing to avoid surprising partial shares.
            for envelope in completed { remove(envelope) }
            throw error
        }
    }

    private func persist(
        _ provided: ProvidedPDF,
        fallbackName: String?,
        sourceApplication: String?,
        processingOptions: [String: String]?
    ) throws -> IntakeRequestEnvelope {
        let identifier = UUID()
        let stagedURL = directories.staged.appendingPathComponent("\(identifier.uuidString).pdf")
        let displayName: String
        switch provided {
        case .file(let source, let suppliedName):
            displayName = suppliedName ?? fallbackName ?? source.lastPathComponent
            try coordinatedCopy(from: source, to: stagedURL)
        case .data(let data, let suppliedName):
            guard Int64(data.count) <= maximumBytes else {
                throw IntakeError.inputTooLarge(limit: maximumBytes)
            }
            displayName = suppliedName ?? fallbackName ?? "Shared PDF.pdf"
            try data.write(to: stagedURL, options: .atomic)
        }
        do {
            try validatePDF(at: stagedURL)
            let envelope = IntakeRequestEnvelope(
                requestIdentifier: identifier,
                stagedFileURL: stagedURL,
                originalDisplayName: displayName,
                sourceApplication: sourceApplication,
                processingOptions: processingOptions
            )
            let requestURL = directories.requests.appendingPathComponent("\(identifier.uuidString).json")
            try encoder.encode(envelope).write(to: requestURL, options: .atomic)
            return envelope
        } catch {
            try? fileManager.removeItem(at: stagedURL)
            throw error
        }
    }

    private func coordinatedCopy(from source: URL, to destination: URL) throws {
        #if os(macOS)
            var coordinationError: NSError?
            var copyError: Error?
            NSFileCoordinator().coordinate(
                readingItemAt: source,
                options: .withoutChanges,
                error: &coordinationError
            ) { coordinatedURL in
                do { try fileManager.copyItem(at: coordinatedURL, to: destination) } catch {
                    copyError = error
                }
            }
            if let error = coordinationError ?? copyError as NSError? { throw error }
        #else
            try fileManager.copyItem(at: source, to: destination)
        #endif
    }

    private func validatePDF(at url: URL) throws {
        let values = try url.resourceValues(forKeys: [.fileSizeKey])
        guard Int64(values.fileSize ?? 0) <= maximumBytes else {
            throw IntakeError.inputTooLarge(limit: maximumBytes)
        }
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        guard try handle.read(upToCount: 5) == Data("%PDF-".utf8) else {
            throw IntakeError.unsupportedInput(url.lastPathComponent)
        }
    }

    private func remove(_ envelope: IntakeRequestEnvelope) {
        try? fileManager.removeItem(at: envelope.stagedFileURL)
        try? fileManager.removeItem(
            at: directories.requests.appendingPathComponent(
                "\(envelope.requestIdentifier.uuidString).json"
            )
        )
    }
}
