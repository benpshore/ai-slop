import Foundation

public protocol DurableIntakeQueue: Sendable {
    /// Must return only after the request has been durably committed.
    /// `false` means it was already present and is therefore safe to acknowledge.
    func insertIfAbsent(_ request: IntakeRequestEnvelope) async throws -> Bool
}

public actor HostIntake {
    private let directories: IntakeDirectories
    private let queue: any DurableIntakeQueue
    private let fileManager: FileManager
    private let decoder = JSONDecoder()

    public init(
        directories: IntakeDirectories,
        queue: any DurableIntakeQueue,
        fileManager: FileManager = .default
    ) throws {
        self.directories = directories
        self.queue = queue
        self.fileManager = fileManager
        try directories.create(fileManager: fileManager)
    }

    /// Replays every unacknowledged envelope, including ones abandoned by a prior launch.
    public func recoverAndIngest() async -> [Result<UUID, Error>] {
        let urls = (try? fileManager.contentsOfDirectory(
            at: directories.requests,
            includingPropertiesForKeys: nil
        ))?.filter { $0.pathExtension == "json" }.sorted { $0.lastPathComponent < $1.lastPathComponent } ?? []
        var results: [Result<UUID, Error>] = []
        for url in urls {
            do { results.append(.success(try await ingest(envelopeAt: url))) } catch {
                results.append(.failure(error))
            }
        }
        return results
    }

    private func ingest(envelopeAt url: URL) async throws -> UUID {
        let envelope = try decoder.decode(IntakeRequestEnvelope.self, from: Data(contentsOf: url))
        guard envelope.version == IntakeRequestEnvelope.currentVersion,
            envelope.stagedFileURL.deletingLastPathComponent().standardizedFileURL
                == directories.staged.standardizedFileURL
        else { throw IntakeError.invalidEnvelope }

        _ = try await queue.insertIfAbsent(envelope)
        // This marker is the acknowledgement. It is atomically published before cleanup.
        let acknowledgement = directories.acknowledgements.appendingPathComponent(
            "\(envelope.requestIdentifier.uuidString).ack"
        )
        try Data().write(to: acknowledgement, options: .atomic)
        try? fileManager.removeItem(at: envelope.stagedFileURL)
        try? fileManager.removeItem(at: url)
        return envelope.requestIdentifier
    }
}

/// A compact reference implementation; production queues can implement the same contract with SQLite.
public actor DirectoryIntakeQueue: DurableIntakeQueue {
    private let directory: URL
    private let encoder = JSONEncoder()

    public init(directory: URL, fileManager: FileManager = .default) throws {
        self.directory = directory
        try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
    }

    public func insertIfAbsent(_ request: IntakeRequestEnvelope) async throws -> Bool {
        let destination = directory.appendingPathComponent("\(request.requestIdentifier.uuidString).json")
        guard !FileManager.default.fileExists(atPath: destination.path) else { return false }
        try encoder.encode(request).write(to: destination, options: .atomic)
        return true
    }
}
