import AppKit
import Observation
import UniformTypeIdentifiers
import WorkspaceCore

enum WorkspaceFilter: String, CaseIterable, Identifiable {
    case all, active, attention
    var id: Self { self }
    var title: String {
        switch self {
        case .all: "All documents"
        case .active: "In progress"
        case .attention: "Needs attention"
        }
    }
    var symbol: String {
        switch self {
        case .all: "square.stack.3d.up"
        case .active: "clock"
        case .attention: "exclamationmark.circle"
        }
    }
}

@MainActor @Observable
final class WorkspaceModel {
    private(set) var queue = WorkspaceQueue()
    var selection: UUID?
    var filter: WorkspaceFilter = .all
    var search = ""
    var notice: String?
    var preferences: ExtractionOptions {
        didSet {
            if let data = try? JSONEncoder().encode(preferences) {
                defaults.set(data, forKey: Self.preferencesKey)
            }
        }
    }
    private(set) var running = false
    @ObservationIgnored private let client: any EngineClient
    @ObservationIgnored private let defaults: UserDefaults
    @ObservationIgnored private var tasks: [UUID: Task<Void, Never>] = [:]
    private static let preferencesKey = "pdftextract.preview.options.v1"

    init(client: any EngineClient = PreviewEngineClient(), defaults: UserDefaults = .standard) {
        self.client = client
        self.defaults = defaults
        self.preferences = defaults.data(forKey: Self.preferencesKey)
            .flatMap { try? JSONDecoder().decode(ExtractionOptions.self, from: $0) }
            ?? ExtractionOptions()
        loadSamples()
    }

    var selectedJob: WorkspaceJob? { queue.jobs.first { $0.id == selection } }
    var queuedCount: Int { queue.jobs.filter { $0.phase == .queued }.count }
    var visibleJobs: [WorkspaceJob] {
        queue.jobs.filter { job in
            let inFilter = switch filter {
            case .all: true
            case .active: job.phase.isActive
            case .attention: job.phase.needsAttention
            }
            return inFilter && (search.isEmpty || job.source.name.localizedCaseInsensitiveContains(search))
        }
    }

    func count(_ filter: WorkspaceFilter) -> Int {
        queue.jobs.filter {
            switch filter {
            case .all: true
            case .active: $0.phase.isActive
            case .attention: $0.phase.needsAttention
            }
        }.count
    }

    func chooseFiles() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.pdf]
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.prompt = "Add to workspace"
        panel.message = "Add PDFs to the offline preview. Files will not be extracted or sent anywhere."
        if panel.runModal() == .OK { addFiles(panel.urls) }
    }

    func addFiles(_ urls: [URL]) {
        var valid: [URL] = []
        var invalid = 0
        for url in urls {
            guard url.isFileURL,
                let values = try? url.resourceValues(forKeys: [.isRegularFileKey, .isReadableKey]),
                values.isRegularFile == true, values.isReadable == true
            else {
                invalid += 1
                continue
            }
            valid.append(url)
        }
        let summary = queue.add(files: valid, options: preferences)
        filter = .all
        search = ""
        if summary.added > 0 { selection = queue.jobs.last?.id }
        var details = ["\(summary.added) PDF\(summary.added == 1 ? "" : "s") added. Preview results use sample data."]
        if summary.duplicates > 0 { details.append("\(summary.duplicates) already in the workspace.") }
        let rejected = invalid + summary.rejected.count
        if rejected > 0 { details.append("\(rejected) unreadable or unsupported item\(rejected == 1 ? "" : "s") skipped.") }
        notice = details.joined(separator: " ")
    }

    func addSample(_ scenario: PreviewScenario) {
        if let id = queue.add(source: DocumentSource(sample: scenario), options: preferences) {
            selection = id
            filter = .all
            search = ""
        } else {
            notice = "That sample is already in the workspace. Select it to inspect or run it again."
        }
    }

    func loadSamples() {
        for scenario in PreviewScenario.allCases {
            guard let id = queue.add(source: DocumentSource(sample: scenario), options: preferences),
                let request = queue.start(id: id) else { continue }
            if scenario == .failure {
                queue.fail(id: id, message: PreviewError.unreadableSample.localizedDescription)
            } else {
                queue.finish(id: id, result: PreviewEngineClient.result(for: request))
            }
        }
        selection = queue.jobs.first?.id
    }

    func runPreview() {
        running = true
        runNext()
    }

    private func runNext() {
        guard running, tasks.isEmpty else { return }
        guard let next = queue.jobs.first(where: { $0.phase == .queued }),
            let request = queue.start(id: next.id)
        else {
            running = false
            return
        }
        let id = request.id
        tasks[id] = Task { [weak self, client] in
            do {
                let result = try await client.run(request) { [weak self] progress in
                    await self?.update(id: id, progress: progress)
                }
                self?.queue.finish(id: id, result: result)
            } catch is CancellationError {
                self?.queue.cancel(id: id)
            } catch {
                self?.queue.fail(id: id, message: error.localizedDescription)
            }
            self?.tasks[id] = nil
            self?.runNext()
        }
    }

    private func update(id: UUID, progress: JobProgress) { queue.update(id: id, progress: progress) }

    func cancel(_ id: UUID) {
        queue.cancel(id: id)
        tasks[id]?.cancel()
    }

    func cancelAll() {
        running = false
        for job in queue.jobs where job.phase.isActive { cancel(job.id) }
    }

    func retry(_ id: UUID) {
        if let replacement = queue.retry(id: id, options: preferences) {
            selection = replacement
            runPreview()
        }
    }

    func remove(_ id: UUID) {
        queue.remove(id: id)
        reconcileSelection()
    }

    func clearFinished() {
        queue.clearFinished()
        reconcileSelection()
    }

    private func reconcileSelection() {
        if !queue.jobs.contains(where: { $0.id == selection }) { selection = queue.jobs.first?.id }
    }

    func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }

    func exportSample(_ job: WorkspaceJob) {
        guard let result = job.result else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [.json]
        panel.nameFieldStringValue = "\(job.source.name.replacingOccurrences(of: ".pdf", with: "")).sample.json"
        panel.message = "Export simulated results. This file contains sample data, not extracted PDF content."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try encoder.encode(result).write(to: url, options: .atomic)
            notice = "Sample JSON exported to \(url.lastPathComponent)."
        } catch {
            notice = "Could not export the sample: \(error.localizedDescription)"
        }
    }
}
