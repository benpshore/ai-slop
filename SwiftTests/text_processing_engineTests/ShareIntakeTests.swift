import Foundation
import Testing
@testable import ShareIntake

private struct Provider: PDFItemProviding {
    let suggestedName: String?
    let result: @Sendable () async throws -> ProvidedPDF
    func loadPDF() async throws -> ProvidedPDF { try await result() }
}

private actor NotificationRecorder: HostNotifying {
    private(set) var batches: [[UUID]] = []
    func notifyHost(requests: [UUID]) { batches.append(requests) }
}

private actor QueueRecorder: DurableIntakeQueue {
    private(set) var requests: [UUID: IntakeRequestEnvelope] = [:]
    var failure: Error?
    func insertIfAbsent(_ request: IntakeRequestEnvelope) throws -> Bool {
        if let failure { throw failure }
        return requests.updateValue(request, forKey: request.requestIdentifier) == nil
    }
}

private struct TestFailure: Error {}

private func fixture(_ suffix: String = UUID().uuidString) throws -> (URL, IntakeDirectories) {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("intake-\(suffix)")
    try? FileManager.default.removeItem(at: root)
    let directories = IntakeDirectories(root: root)
    try directories.create()
    return (root, directories)
}

private let pdf = Data("%PDF-1.7\n%%EOF".utf8)

@Test func stagesFileBackedProviderAndPreservesMetadata() async throws {
    let (root, directories) = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let source = root.appendingPathComponent("original.pdf")
    try pdf.write(to: source)
    let stager = try IntakeStager(directories: directories)
    let envelopes = try await stager.stage(
        providers: [Provider(suggestedName: nil) { .file(source, displayName: "Paper.pdf") }],
        sourceApplication: "com.example.source",
        processingOptions: ["ocr": "auto"]
    )
    #expect(envelopes.count == 1)
    #expect(envelopes[0].originalDisplayName == "Paper.pdf")
    #expect(envelopes[0].sourceApplication == "com.example.source")
    #expect(envelopes[0].processingOptions == ["ocr": "auto"])
    #expect(FileManager.default.fileExists(atPath: envelopes[0].stagedFileURL.path))
}

@Test func stagesMultipleDataBackedPDFsAndNotifiesAfterPersistence() async throws {
    let (root, directories) = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let notifier = NotificationRecorder()
    let providers = (1...3).map { index in
        Provider(suggestedName: "\(index).pdf") { .data(pdf, displayName: nil) }
    }
    let envelopes = try await IntakeStager(directories: directories).stage(
        providers: providers,
        notifier: notifier
    )
    #expect(envelopes.count == 3)
    #expect(await notifier.batches == [envelopes.map(\.requestIdentifier)])
    #expect(try FileManager.default.contentsOfDirectory(atPath: directories.requests.path).count == 3)
}

@Test func providerFailureAndCancellationLeaveNoPartialBatch() async throws {
    let (root, directories) = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let stager = try IntakeStager(directories: directories)
    let good = Provider(suggestedName: "good.pdf") { .data(pdf, displayName: nil) }
    let failed = Provider(suggestedName: "bad.pdf") { throw TestFailure() }
    await #expect(throws: IntakeError.self) { try await stager.stage(providers: [good, failed]) }
    #expect(try FileManager.default.contentsOfDirectory(atPath: directories.staged.path).isEmpty)
    let cancelled = Provider(suggestedName: "cancel.pdf") { throw CancellationError() }
    await #expect(throws: IntakeError.cancelled) { try await stager.stage(providers: [cancelled]) }
}

@Test func rejectsNonPDFAndOversizedDataWithVisibleErrors() async throws {
    let (root, directories) = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let stager = try IntakeStager(directories: directories, maximumBytes: 12)
    await #expect(throws: IntakeError.self) {
        try await stager.stage(providers: [Provider(suggestedName: "fake.pdf") {
            .data(Data("not a pdf".utf8), displayName: nil)
        }])
    }
    await #expect(throws: IntakeError.inputTooLarge(limit: 12)) {
        try await stager.stage(providers: [Provider(suggestedName: "large.pdf") {
            .data(pdf, displayName: nil)
        }])
    }
}

@Test func hostRecoversWithoutNotificationAndCleansOnlyAfterDurableInsert() async throws {
    let (root, directories) = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let envelope = try await IntakeStager(directories: directories).stage(
        providers: [Provider(suggestedName: "offline.pdf") { .data(pdf, displayName: nil) }]
    )[0]
    let queue = QueueRecorder()
    let host = try HostIntake(directories: directories, queue: queue)
    let results = await host.recoverAndIngest()
    #expect(results.count == 1)
    #expect(await queue.requests.count == 1)
    #expect(!FileManager.default.fileExists(atPath: envelope.stagedFileURL.path))
    #expect(FileManager.default.fileExists(
        atPath: directories.acknowledgements.appendingPathComponent(
            "\(envelope.requestIdentifier.uuidString).ack"
        ).path
    ))
}

@Test func duplicateDeliveryIsIdempotentAndFailedInsertionIsRecoverable() async throws {
    let (root, directories) = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let envelope = try await IntakeStager(directories: directories).stage(
        providers: [Provider(suggestedName: "again.pdf") { .data(pdf, displayName: nil) }]
    )[0]
    let requestURL = directories.requests.appendingPathComponent(
        "\(envelope.requestIdentifier.uuidString).json"
    )
    let bytes = try Data(contentsOf: requestURL)
    let queue = QueueRecorder()
    let host = try HostIntake(directories: directories, queue: queue)
    _ = await host.recoverAndIngest()
    try bytes.write(to: requestURL, options: .atomic)
    _ = await host.recoverAndIngest()
    #expect(await queue.requests.count == 1)

    let second = try await IntakeStager(directories: directories).stage(
        providers: [Provider(suggestedName: "retry.pdf") { .data(pdf, displayName: nil) }]
    )[0]
    let failingQueue = QueueRecorder()
    await failingQueue.setFailure(TestFailure())
    let failingHost = try HostIntake(directories: directories, queue: failingQueue)
    _ = await failingHost.recoverAndIngest()
    #expect(FileManager.default.fileExists(atPath: second.stagedFileURL.path))
}

private extension QueueRecorder {
    func setFailure(_ error: Error?) { failure = error }
}
