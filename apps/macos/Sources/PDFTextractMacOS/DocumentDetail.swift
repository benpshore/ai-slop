import AppKit
import PDFKit
import SwiftUI
import WorkspaceCore

private enum DetailTab: String, CaseIterable, Identifiable {
    case text = "Text", references = "References", source = "Source PDF", activity = "Details"
    var id: Self { self }
}

struct DocumentDetail: View {
    let job: WorkspaceJob
    let model: WorkspaceModel
    @State private var tab: DetailTab = .text

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Picker("Document view", selection: $tab) {
                ForEach(DetailTab.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented).padding(.horizontal, 24).padding(.bottom, 18)
            Divider()
            Group {
                switch tab {
                case .source: sourceView
                case .activity: details
                case .text: resultBody { textContent }
                case .references: resultBody { referenceContent }
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(Color(nsColor: .textBackgroundColor))
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 13) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 8) {
                    Text(job.source.name).font(.title2.weight(.semibold)).textSelection(.enabled)
                    HStack(spacing: 12) {
                        StatusBadge(phase: job.phase)
                        Text(job.options.mode.title).font(.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer()
                Menu {
                    if job.phase.isActive {
                        Button("Cancel preview") { model.cancel(job.id) }
                    } else {
                        Button("Run preview again") { model.retry(job.id) }
                        Button("Remove from workspace") { model.remove(job.id) }
                    }
                    if job.result != nil {
                        Divider()
                        Button("Export sample JSON…") { model.exportSample(job) }
                    }
                    if let url = job.source.fileURL {
                        Button("Reveal source in Finder") { NSWorkspace.shared.activateFileViewerSelecting([url]) }
                    }
                } label: { Image(systemName: "ellipsis.circle").font(.title3) }
                .menuStyle(.borderlessButton).fixedSize().accessibilityLabel("Document actions")
            }
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "info.circle")
                Text("Results below are fictional sample data. The engine is not connected; imported PDFs are not extracted.")
            }
            .font(.caption).foregroundStyle(.secondary)
            .padding(11).frame(maxWidth: .infinity, alignment: .leading)
            .background(.teal.opacity(0.06), in: RoundedRectangle(cornerRadius: 8))
            if let result = job.result, result.completeness == .limited {
                Label("Incomplete sample — some content is missing.", systemImage: "exclamationmark.triangle.fill")
                    .font(.callout.weight(.medium)).foregroundStyle(.orange)
            }
        }
        .padding(24)
    }

    @ViewBuilder private func resultBody<Content: View>(@ViewBuilder content: () -> Content) -> some View {
        switch job.phase {
        case .finished:
            content()
        case .queued:
            ContentUnavailableView {
                Label("Ready to preview", systemImage: "play.circle")
            } description: {
                Text("Run the preview to see sample results for this workflow. Your PDF will not be processed.")
            } actions: {
                Button("Run Preview", action: model.runPreview).buttonStyle(.borderedProminent)
            }
        case .running(let progress):
            VStack(spacing: 14) {
                ProgressView(value: progress.fraction).frame(width: 220)
                Text(progress.message).foregroundStyle(.secondary)
                Button("Cancel preview") { model.cancel(job.id) }
            }
        case .failed(let message):
            ContentUnavailableView {
                Label("Sample job failed", systemImage: "exclamationmark.triangle")
            } description: { Text(message) } actions: {
                Button("Run preview again") { model.retry(job.id) }
            }
        case .cancelled:
            ContentUnavailableView {
                Label("Preview cancelled", systemImage: "stop.circle")
            } description: { Text("No extracted result was produced.") } actions: {
                Button("Run preview again") { model.retry(job.id) }
            }
        }
    }

    @ViewBuilder private var textContent: some View {
        if let text = job.result?.text {
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    HStack {
                        Label("Sample text", systemImage: "text.alignleft")
                            .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                        Spacer()
                        Button { model.copy(text) } label: { Label("Copy sample", systemImage: "doc.on.doc") }
                            .controlSize(.small)
                    }
                    Text(text).font(.system(.body, design: .serif)).lineSpacing(7)
                        .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                }
                .padding(28)
            }
        } else {
            ContentUnavailableView("Text was not requested", systemImage: "text.alignleft",
                description: Text("Choose Text or Text and bibliography in Settings, then run the preview again."))
        }
    }

    @ViewBuilder private var referenceContent: some View {
        if let references = job.result?.references {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    HStack {
                        Text("\(references.count) sample entries").font(.headline)
                        Spacer()
                        Text("Resolution not measured").font(.caption).foregroundStyle(.secondary)
                    }
                    Text("These fictional citations demonstrate the interface. No DOI, PMID or PMCID has been verified.")
                        .font(.callout).foregroundStyle(.secondary)
                    ForEach(references) { entry in
                        HStack(alignment: .top, spacing: 14) {
                            Text("\(entry.id)").font(.caption.monospacedDigit()).foregroundStyle(.teal)
                                .frame(width: 26, height: 26)
                                .background(.teal.opacity(0.08), in: RoundedRectangle(cornerRadius: 7))
                            VStack(alignment: .leading, spacing: 7) {
                                Text(entry.title).font(.headline)
                                Text(entry.authors).font(.callout)
                                Text(entry.publication).font(.caption).foregroundStyle(.secondary)
                                Text(entry.identifierVerification).font(.caption2).foregroundStyle(.secondary)
                            }
                            .textSelection(.enabled)
                            Spacer(minLength: 0)
                        }
                        .padding(16).frame(maxWidth: .infinity, alignment: .leading)
                        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 12))
                    }
                }
                .padding(26)
            }
        } else {
            ContentUnavailableView("Bibliography was not requested", systemImage: "list.bullet.rectangle",
                description: Text("Choose Bibliography or Text and bibliography in Settings, then run the preview again."))
        }
    }

    @ViewBuilder private var sourceView: some View {
        if let url = job.source.fileURL {
            VStack(spacing: 0) {
                HStack {
                    Label("Local preview using macOS PDFKit", systemImage: "doc")
                    Spacer()
                }
                .font(.caption).foregroundStyle(.secondary).padding(12)
                SourcePDFView(url: url).id(url)
            }
        } else {
            ContentUnavailableView("This is a sample document", systemImage: "doc.badge.ellipsis",
                description: Text("Add a PDF to preview its pages here. Viewing a source PDF does not run extraction."))
        }
    }

    private var details: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                GroupBox("Job configuration") {
                    VStack(spacing: 12) {
                        detailRow("Requested output", job.options.mode.title)
                        detailRow("Engine preference", job.options.engine.title)
                        detailRow("Line breaks", job.options.preserveLineBreaks ? "Preserved" : "Joined")
                        detailRow("Actual provider", job.result?.backendIdentity ?? "Offline preview")
                        detailRow("Source", job.source.fileURL?.path ?? "Built-in fictional sample")
                    }.padding(10)
                }
                if let result = job.result {
                    GroupBox("Outcome") {
                        VStack(spacing: 12) {
                            detailRow("Completeness", result.completeness.rawValue.capitalized + " (simulated)")
                            detailRow("Pages in sample result", "\(result.pagesProcessed)")
                            detailRow("Identifier resolution", "Not measured")
                        }.padding(10)
                    }
                    VStack(alignment: .leading, spacing: 10) {
                        Text("Warnings").font(.headline)
                        if result.warnings.isEmpty {
                            Text("No warnings in this sample.").foregroundStyle(.secondary)
                        }
                        ForEach(result.warnings, id: \.self) { warning in
                            Label(warning, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                        }
                    }
                    .font(.callout)
                }
                Text("Settings apply when a document is added or a preview is retried. Engine preferences are saved for future integration; every result in this app currently comes from the offline preview provider.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .padding(24)
        }
    }

    private func detailRow(_ title: String, _ value: String) -> some View {
        HStack(alignment: .top, spacing: 18) {
            Text(title).foregroundStyle(.secondary).frame(width: 135, alignment: .leading)
            Text(value).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
        }
        .font(.callout)
    }
}

private struct SourcePDFView: NSViewRepresentable {
    let url: URL

    func makeNSView(context: Context) -> PDFView {
        let view = PDFView()
        view.autoScales = true
        view.displayMode = .singlePageContinuous
        view.displayDirection = .vertical
        view.document = PDFDocument(url: url)
        return view
    }

    func updateNSView(_ view: PDFView, context: Context) {}
}
