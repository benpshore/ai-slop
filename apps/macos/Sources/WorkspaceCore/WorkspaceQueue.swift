import Foundation

public enum JobPhase: Equatable, Sendable {
    case queued
    case running(JobProgress)
    case finished(ExtractionResult)
    case failed(String)
    case cancelled

    public var isActive: Bool {
        switch self {
        case .queued, .running: true
        default: false
        }
    }

    public var needsAttention: Bool {
        switch self {
        case .failed: true
        case .finished(let result): result.completeness == .limited
        default: false
        }
    }
}

public struct WorkspaceJob: Identifiable, Equatable, Sendable {
    public let id: UUID
    public let source: DocumentSource
    public let options: ExtractionOptions
    public var phase: JobPhase

    public var request: ExtractionRequest {
        ExtractionRequest(id: id, source: source, options: options)
    }

    public var result: ExtractionResult? {
        if case .finished(let result) = phase { return result }
        return nil
    }
}

public struct IntakeSummary: Equatable, Sendable {
    public var added: Int = 0
    public var duplicates: Int = 0
    public var rejected: [String] = []
}

/// Deterministic UI state with guarded transitions: a stale completion cannot undo cancellation.
public struct WorkspaceQueue: Sendable {
    public private(set) var jobs: [WorkspaceJob] = []

    public init() {}

    public mutating func add(files: [URL], options: ExtractionOptions) -> IntakeSummary {
        var summary = IntakeSummary()
        for url in files {
            guard url.isFileURL, url.pathExtension.lowercased() == "pdf" else {
                summary.rejected.append(url.lastPathComponent)
                continue
            }
            if add(source: DocumentSource(fileURL: url), options: options) == nil {
                summary.duplicates += 1
            } else {
                summary.added += 1
            }
        }
        return summary
    }

    @discardableResult
    public mutating func add(source: DocumentSource, options: ExtractionOptions) -> UUID? {
        guard !jobs.contains(where: { $0.source.identity == source.identity }) else { return nil }
        let id = UUID()
        jobs.append(WorkspaceJob(id: id, source: source, options: options, phase: .queued))
        return id
    }

    public mutating func start(id: UUID) -> ExtractionRequest? {
        guard let index = index(id), jobs[index].phase == .queued else { return nil }
        jobs[index].phase = .running(JobProgress(completedPages: 0, totalPages: nil, message: "Preparing sample"))
        return jobs[index].request
    }

    public mutating func update(id: UUID, progress: JobProgress) {
        guard let index = index(id), case .running(let previous) = jobs[index].phase else { return }
        guard progress.completedPages >= previous.completedPages else { return }
        jobs[index].phase = .running(progress)
    }

    public mutating func finish(id: UUID, result: ExtractionResult) {
        guard let index = index(id), case .running = jobs[index].phase else { return }
        jobs[index].phase = .finished(result)
    }

    public mutating func fail(id: UUID, message: String) {
        guard let index = index(id), case .running = jobs[index].phase else { return }
        jobs[index].phase = .failed(message)
    }

    public mutating func cancel(id: UUID) {
        guard let index = index(id), jobs[index].phase.isActive else { return }
        jobs[index].phase = .cancelled
    }

    /// A retry gets a new identity so late events from its predecessor cannot reach it.
    @discardableResult
    public mutating func retry(id: UUID, options: ExtractionOptions) -> UUID? {
        guard let index = index(id), !jobs[index].phase.isActive else { return nil }
        let replacement = UUID()
        jobs[index] = WorkspaceJob(
            id: replacement, source: jobs[index].source, options: options, phase: .queued
        )
        return replacement
    }

    public mutating func remove(id: UUID) {
        guard let index = index(id), !jobs[index].phase.isActive else { return }
        jobs.remove(at: index)
    }

    public mutating func clearFinished() {
        jobs.removeAll { !$0.phase.isActive }
    }

    private func index(_ id: UUID) -> Int? {
        jobs.firstIndex { $0.id == id }
    }
}
