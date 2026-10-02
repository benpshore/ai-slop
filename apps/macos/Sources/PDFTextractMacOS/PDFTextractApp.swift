import AppKit
import SwiftUI
import WorkspaceCore

@main
struct PDFTextractApp: App {
    @State private var model = WorkspaceModel()

    var body: some Scene {
        Window("PDFTextract Preview", id: "workspace") {
            WorkspaceView(model: model)
                .frame(minWidth: 1080, minHeight: 680)
                .onOpenURL { model.addFiles([$0]) }
                .task { await captureSmokeSnapshotIfRequested() }
        }
        .defaultSize(width: 1320, height: 820)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("Add PDFs…", action: model.chooseFiles).keyboardShortcut("o")
                Divider()
                Button("Run Preview", action: model.runPreview)
                    .keyboardShortcut("r")
                    .disabled(model.queuedCount == 0 || model.running)
                Button("Cancel All", action: model.cancelAll)
                    .keyboardShortcut(".")
                    .disabled(!model.queue.jobs.contains { $0.phase.isActive })
            }
        }
        Settings {
            SettingsView(model: model).frame(width: 490)
        }
    }

    /// CI-only view rendering smoke test; it does not exercise or claim engine functionality.
    @MainActor private func captureSmokeSnapshotIfRequested() async {
        guard let path = ProcessInfo.processInfo.environment["PDFTEXTRACT_UI_SMOKE_SNAPSHOT"] else { return }
        do {
            NSApplication.shared.activate()
            try await Task.sleep(for: .seconds(2))
            guard let window = NSApplication.shared.windows.first(where: { $0.isVisible }),
                let view = window.contentView?.superview,
                let bitmap = view.bitmapImageRepForCachingDisplay(in: view.bounds)
            else { throw CocoaError(.coderValueNotFound) }
            view.cacheDisplay(in: view.bounds, to: bitmap)
            guard let data = bitmap.representation(using: .png, properties: [:]) else {
                throw CocoaError(.fileWriteUnknown)
            }
            try data.write(to: URL(fileURLWithPath: path), options: .atomic)
            NSApplication.shared.terminate(nil)
        } catch {
            FileHandle.standardError.write(Data("UI smoke test failed: \(error)\n".utf8))
            exit(1)
        }
    }
}
