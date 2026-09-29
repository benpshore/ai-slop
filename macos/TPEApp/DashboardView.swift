import SwiftUI
import UniformTypeIdentifiers

struct DashboardView: View {
    @Bindable var model: IntakeModel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @FocusState private var openFocused: Bool

    var body: some View {
        NavigationSplitView {
            List(model.records) { record in
                VStack(alignment: .leading, spacing: 4) {
                    Text(record.filename).font(.headline)
                    Text(record.receivedAt, style: .relative).font(.subheadline).foregroundStyle(.secondary)
                }
                .accessibilityElement(children: .combine)
                .accessibilityLabel(record.filename)
                .accessibilityValue("Imported \(record.receivedAt.formatted(.relative(presentation: .named)))")
            }
            .navigationTitle("Documents")
        } detail: {
            VStack(spacing: 20) {
                Image(systemName: "doc.text.magnifyingglass")
                    .font(.system(size: dynamicTypeSize.isAccessibilitySize ? 64 : 44))
                    .accessibilityHidden(true)
                Text("Add research PDFs").font(.largeTitle).bold()
                Text("Open a file, drag a PDF here, or use Share → Text Processing Engine.")
                    .multilineTextAlignment(.center)
                Button("Open PDF…") { model.showOpenPanel() }
                    .buttonStyle(.borderedProminent)
                    .focused($openFocused)
                    .keyboardShortcut(.defaultAction)
                    .accessibilityHint("Shows a file chooser for one or more PDF documents")
                if let progress = model.progress {
                    ProgressView(value: progress) { Text("Import progress") }
                        .accessibilityValue(Text("\(Int(progress * 100)) percent"))
                        .animation(reduceMotion ? nil : .default, value: progress)
                }
                Text(model.status)
                    .foregroundStyle(.secondary)
                    .accessibilityLabel("Status")
                    .accessibilityValue(model.status)
            }
            .padding(32)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .contentShape(Rectangle())
            .dropDestination(for: URL.self) { urls, _ in
                let PDFs = urls.filter { $0.pathExtension.caseInsensitiveCompare("pdf") == .orderedSame }
                PDFs.forEach { model.accept($0, source: "Drag and Drop") }
                return !PDFs.isEmpty
            }
            .accessibilityAction(named: "Open PDF") { model.showOpenPanel() }
            .task { openFocused = true }
        }
    }
}
