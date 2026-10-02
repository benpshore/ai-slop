import SwiftUI
import WorkspaceCore

struct SettingsView: View {
    @Bindable var model: WorkspaceModel

    var body: some View {
        Form {
            Section {
                Label("Offline preview", systemImage: "network.slash").font(.headline)
                Text("Configure the workspace independently of the engine. There is no API connection in this app yet.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            Section("Defaults for new jobs") {
                Picker("Output", selection: $model.preferences.mode) {
                    ForEach(ExtractionMode.allCases) { Text($0.title).tag($0) }
                }
                Toggle("Preserve paragraph breaks in sample text", isOn: $model.preferences.preserveLineBreaks)
                Picker("Preferred engine", selection: $model.preferences.engine) {
                    ForEach(EnginePreference.allCases) { Text($0.title).tag($0) }
                }
                Text("Engine preference is saved only. The preview always uses fictional data; it does not load PDFium, Docling or lopdf.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Local workspace") {
                Text("Preferences are stored on this Mac. The document list and sample results last for this app session. Original PDFs are never modified.")
                    .font(.callout).foregroundStyle(.secondary)
                Button("Restore default settings") { model.preferences = ExtractionOptions() }
            }
        }
        .formStyle(.grouped).padding(12)
    }
}
