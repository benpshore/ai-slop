import Foundation
import Testing
@testable import WorkspaceCore

private func enqueue(_ scenario: PreviewScenario = .complete, into queue: inout WorkspaceQueue) throws -> UUID {
    let candidate = queue.add(source: DocumentSource(sample: scenario), options: ExtractionOptions())
    return try #require(candidate)
}

private func start(_ id: UUID, in queue: inout WorkspaceQueue) throws -> ExtractionRequest {
    let candidate = queue.start(id: id)
    return try #require(candidate)
}

@Test func intakeRejectsRemoteAndNonPDFItemsAndDeduplicatesPaths() {
    var queue = WorkspaceQueue()
    let summary = queue.add(files: [
        URL(fileURLWithPath: "/tmp/Paper.PDF"),
        URL(fileURLWithPath: "/tmp/./Paper.PDF"),
        URL(fileURLWithPath: "/tmp/notes.txt"),
        URL(string: "https://example.invalid/paper.pdf")!,
    ], options: ExtractionOptions())
    #expect(summary.added == 1)
    #expect(summary.duplicates == 1)
    #expect(summary.rejected.count == 2)
    #expect(queue.jobs.count == 1)
}

@Test func optionsAreCapturedAtEnqueueTime() throws {
    var queue = WorkspaceQueue()
    var options = ExtractionOptions(mode: .bibliography, engine: .pdfium)
    queue.add(source: DocumentSource(sample: .complete), options: options)
    options.mode = .text
    #expect(try #require(queue.jobs.first).options.mode == .bibliography)
    #expect(try #require(queue.jobs.first).options.engine == .pdfium)
}

@Test func cancelledJobsIgnoreLateProgressCompletionAndFailure() throws {
    var queue = WorkspaceQueue()
    let id = try enqueue(into: &queue)
    let request = try start(id, in: &queue)
    queue.cancel(id: id)
    queue.update(id: id, progress: JobProgress(completedPages: 4, totalPages: 4, message: "Late"))
    queue.finish(id: id, result: PreviewEngineClient.result(for: request))
    queue.fail(id: id, message: "Late error")
    #expect(queue.jobs.first?.phase == .cancelled)
}

@Test func queuedJobsCannotAcceptCompletionWithoutStarting() throws {
    var queue = WorkspaceQueue()
    let id = try enqueue(into: &queue)
    let request = try #require(queue.jobs.first).request
    queue.finish(id: id, result: PreviewEngineClient.result(for: request))
    #expect(queue.jobs.first?.phase == .queued)
}

@Test func retriesUseNewIdentityAndIgnorePredecessorEvents() throws {
    var queue = WorkspaceQueue()
    let oldID = try enqueue(into: &queue)
    let oldRequest = try start(oldID, in: &queue)
    queue.cancel(id: oldID)
    let retried = queue.retry(id: oldID, options: ExtractionOptions(mode: .text))
    let newID = try #require(retried)
    #expect(newID != oldID)
    _ = try start(newID, in: &queue)
    queue.finish(id: oldID, result: PreviewEngineClient.result(for: oldRequest))
    #expect(queue.jobs.first?.id == newID)
    #expect(queue.jobs.first?.phase.isActive == true)
    #expect(queue.jobs.first?.options.mode == .text)
}

@Test func activeJobsCannotBeRemovedOrRetried() throws {
    var queue = WorkspaceQueue()
    let id = try enqueue(into: &queue)
    queue.remove(id: id)
    let retried = queue.retry(id: id, options: ExtractionOptions())
    #expect(retried == nil)
    queue.clearFinished()
    #expect(queue.jobs.count == 1)
    queue.cancel(id: id)
    queue.clearFinished()
    #expect(queue.jobs.isEmpty)
}

@Test func progressIsClampedAndDoesNotMoveBackwards() throws {
    var queue = WorkspaceQueue()
    let id = try enqueue(into: &queue)
    _ = try start(id, in: &queue)
    queue.update(id: id, progress: JobProgress(completedPages: 3, totalPages: 4, message: "Current"))
    queue.update(id: id, progress: JobProgress(completedPages: 2, totalPages: 4, message: "Stale"))
    let phase = try #require(queue.jobs.first).phase
    guard case .running(let progress) = phase else {
        Issue.record("Expected a running job")
        return
    }
    #expect(progress.completedPages == 3)
    #expect(progress.fraction == 0.75)
    #expect(JobProgress(completedPages: 9, totalPages: 4, message: "").fraction == 1)
    #expect(JobProgress(completedPages: -1, totalPages: 0, message: "").fraction == nil)
}

@Test func limitedResultsRemainDistinctAndKeepWarningsInJSON() throws {
    let request = ExtractionRequest(id: UUID(), source: DocumentSource(sample: .limited), options: ExtractionOptions())
    let result = PreviewEngineClient.result(for: request)
    #expect(result.completeness == .limited)
    #expect(!result.warnings.isEmpty)
    #expect(JobPhase.finished(result).needsAttention)
    let encoded = try JSONEncoder().encode(result)
    let decoded = try JSONDecoder().decode(ExtractionResult.self, from: encoded)
    #expect(decoded == result)
    #expect(decoded.isSimulated)
    #expect(decoded.backendIdentity == "offline-preview/1")
}

@Test(arguments: ExtractionMode.allCases)
func previewHonorsRequestedOutput(_ mode: ExtractionMode) {
    let request = ExtractionRequest(id: UUID(), source: DocumentSource(sample: .complete), options: ExtractionOptions(mode: mode))
    let result = PreviewEngineClient.result(for: request)
    #expect((result.text != nil) == (mode != .bibliography))
    #expect((result.references != nil) == (mode != .text))
    #expect(result.references?.allSatisfy { $0.identifier == nil } ?? true)
}

@Test func previewDoesNotReadItsInputOrPretendToUseSelectedEngine() async throws {
    let request = ExtractionRequest(
        id: UUID(), source: DocumentSource(fileURL: URL(fileURLWithPath: "/missing/never-opened.pdf")),
        options: ExtractionOptions(engine: .docling)
    )
    let result = try await PreviewEngineClient(stepDelay: .zero).run(request) { _ in }
    #expect(result.isSimulated)
    #expect(result.backendIdentity == "offline-preview/1")
    #expect(result.text?.contains("NOT EXTRACTED FROM YOUR PDF") == true)
}

@Test func previewFailureDoesNotReturnSuccess() async {
    let request = ExtractionRequest(id: UUID(), source: DocumentSource(sample: .failure), options: ExtractionOptions())
    do {
        _ = try await PreviewEngineClient(stepDelay: .zero).run(request) { _ in }
        Issue.record("Expected a simulated failure")
    } catch {
        #expect(error is PreviewError)
    }
}

@Test func previewCancellationIsCooperative() async {
    let request = ExtractionRequest(id: UUID(), source: DocumentSource(sample: .complete), options: ExtractionOptions())
    let task = Task {
        try await PreviewEngineClient(stepDelay: .seconds(60)).run(request) { _ in }
    }
    task.cancel()
    do {
        _ = try await task.value
        Issue.record("Expected cancellation")
    } catch {
        #expect(error is CancellationError)
    }
}

@Test func preferencesRoundTripWithoutChangingEngineDefaults() throws {
    let preferences = ExtractionOptions(mode: .bibliography, engine: .pdfium, preserveLineBreaks: false)
    #expect(try JSONDecoder().decode(ExtractionOptions.self, from: JSONEncoder().encode(preferences)) == preferences)
    #expect(ExtractionOptions().engine == .automatic)
}
