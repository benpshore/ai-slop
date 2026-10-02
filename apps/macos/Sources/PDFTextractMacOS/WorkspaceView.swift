import AppKit
import SwiftUI
import WorkspaceCore

struct WorkspaceView: View {
    @Bindable var model: WorkspaceModel
    @State private var dropTargeted = false

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 190, ideal: 205, max: 240)
        } content: {
            documentList
                .navigationSplitViewColumnWidth(min: 290, ideal: 345, max: 430)
        } detail: {
            if let job = model.selectedJob {
                DocumentDetail(job: job, model: model)
            } else {
                ContentUnavailableView(
                    "Select a document", systemImage: "doc.text.magnifyingglass",
                    description: Text("Inspect sample text, references and job outcomes here.")
                )
            }
        }
        .navigationTitle("PDFTextract Preview")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button(action: model.chooseFiles) { Label("Add PDFs", systemImage: "plus") }
                    .help("Add local PDFs to the offline workspace (⌘O)")
                Button(action: model.runPreview) { Label("Run Preview", systemImage: "play.fill") }
                    .disabled(model.queuedCount == 0 || model.running)
                    .help("Run queued items using sample data (⌘R)")
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "network.slash").foregroundStyle(.secondary)
                Text("Offline preview").fontWeight(.medium)
                Text("·").foregroundStyle(.tertiary)
                Text("Engine disconnected. Results are sample data.").foregroundStyle(.secondary)
                Spacer()
                Text("\(model.queue.jobs.count) documents").foregroundStyle(.secondary)
            }
            .font(.caption)
            .padding(.horizontal, 18).padding(.vertical, 10)
            .background(.bar)
            .overlay(alignment: .top) { Divider() }
        }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: "doc.viewfinder")
                    .font(.title2).foregroundStyle(.teal)
                    .frame(width: 38, height: 38)
                    .background(.teal.opacity(0.1), in: RoundedRectangle(cornerRadius: 10))
                VStack(alignment: .leading, spacing: 2) {
                    Text("PDFTextract").font(.headline)
                    Text("Document workspace").font(.caption).foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 14).padding(.vertical, 22)
            List(selection: $model.filter) {
                Section("Workspace") {
                    ForEach(WorkspaceFilter.allCases) { filter in
                        HStack {
                            Label(filter.title, systemImage: filter.symbol)
                            Spacer()
                            Text("\(model.count(filter))").foregroundStyle(.secondary).monospacedDigit()
                        }
                        .tag(filter)
                    }
                }
                Section("Explore") {
                    Menu {
                        ForEach(PreviewScenario.allCases) { scenario in
                            Button(scenario.title) { model.addSample(scenario) }
                        }
                    } label: { Label("Add a sample", systemImage: "sparkles") }
                    .menuStyle(.borderlessButton)
                    Button(action: model.clearFinished) {
                        Label("Clear finished", systemImage: "tray")
                    }
                    .buttonStyle(.plain)
                    .disabled(!model.queue.jobs.contains { !$0.phase.isActive })
                }
            }
            .listStyle(.sidebar)
            VStack(alignment: .leading, spacing: 9) {
                Label("A separate native app", systemImage: "macwindow").font(.caption.weight(.medium))
                Text("Try the workflow with fictional results. Your PDFs stay on this Mac.")
                    .font(.caption).foregroundStyle(.secondary)
                SettingsLink { Label("Settings", systemImage: "gearshape") }
                    .buttonStyle(.link)
            }
            .padding(16)
        }
    }

    private var documentList: some View {
        VStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                Text(model.filter.title).font(.title2.weight(.semibold))
                Text("Add documents, then preview the workflow.")
                    .font(.callout).foregroundStyle(.secondary)
                Button(action: model.chooseFiles) {
                    HStack(spacing: 12) {
                        Image(systemName: "square.and.arrow.down").font(.title2).foregroundStyle(.teal)
                        VStack(alignment: .leading, spacing: 3) {
                            Text("Drop PDFs here").font(.callout.weight(.medium))
                            Text("or choose files from your Mac").font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                    }
                    .padding(16)
                    .background(dropTargeted ? Color.teal.opacity(0.12) : Color.primary.opacity(0.035),
                        in: RoundedRectangle(cornerRadius: 12))
                    .overlay { RoundedRectangle(cornerRadius: 12).strokeBorder(.teal.opacity(0.4), style: StrokeStyle(lineWidth: 1, dash: [5, 4])) }
                }
                .buttonStyle(.plain)
                .padding(.top, 8)
                .dropDestination(for: URL.self) { urls, _ in
                    model.addFiles(urls)
                    return !urls.isEmpty
                } isTargeted: { dropTargeted = $0 }
            }
            .padding(20)
            if let notice = model.notice {
                HStack(alignment: .top, spacing: 8) {
                    Text(notice).font(.caption).foregroundStyle(.secondary)
                    Spacer(minLength: 0)
                    Button { model.notice = nil } label: { Image(systemName: "xmark") }
                        .buttonStyle(.plain).accessibilityLabel("Dismiss notice")
                }
                .padding(12).background(.teal.opacity(0.05)).padding(.horizontal, 20).padding(.bottom, 12)
            }
            Divider()
            if model.visibleJobs.isEmpty {
                ContentUnavailableView(
                    model.search.isEmpty ? "No documents here" : "No matching documents",
                    systemImage: "tray",
                    description: Text("Add a PDF or choose a different workspace filter.")
                )
            } else {
                List(selection: $model.selection) {
                    ForEach(model.visibleJobs) { job in
                        JobRow(job: job).tag(job.id)
                            .contextMenu {
                                if job.phase.isActive {
                                    Button("Cancel preview") { model.cancel(job.id) }
                                } else {
                                    Button("Run preview again") { model.retry(job.id) }
                                    Button("Remove from workspace") { model.remove(job.id) }
                                }
                            }
                    }
                }
                .listStyle(.inset)
            }
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .searchable(text: $model.search, placement: .automatic, prompt: "Find a document")
    }
}

struct JobRow: View {
    let job: WorkspaceJob

    var body: some View {
        HStack(alignment: .top, spacing: 11) {
            Image(systemName: "doc.richtext").font(.title2).foregroundStyle(.secondary)
                .frame(width: 28).padding(.top, 3)
            VStack(alignment: .leading, spacing: 6) {
                Text(job.source.name).font(.callout.weight(.medium)).lineLimit(2)
                HStack(spacing: 6) {
                    StatusBadge(phase: job.phase)
                    Text(job.source.fileURL == nil ? "Sample" : "Local PDF")
                        .font(.caption2).foregroundStyle(.secondary)
                }
                if case .running(let progress) = job.phase {
                    ProgressView(value: progress.fraction).controlSize(.small)
                    Text(progress.message).font(.caption2).foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 9)
        .accessibilityElement(children: .combine)
    }
}

struct StatusBadge: View {
    let phase: JobPhase
    private var presentation: (String, String, Color) {
        switch phase {
        case .queued: ("Queued", "clock", .secondary)
        case .running: ("Preview running", "arrow.triangle.2.circlepath", .teal)
        case .finished(let result): result.completeness == .limited
            ? ("Limited sample", "exclamationmark.circle.fill", .orange)
            : ("Complete sample", "checkmark.circle.fill", .teal)
        case .failed: ("Failed sample", "xmark.circle.fill", .red)
        case .cancelled: ("Cancelled", "stop.circle", .secondary)
        }
    }
    var body: some View {
        Label(presentation.0, systemImage: presentation.1)
            .font(.caption2.weight(.medium)).foregroundStyle(presentation.2)
    }
}
