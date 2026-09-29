import AppKit
import Observation
import TPEMacSupport

@MainActor @Observable
final class IntakeModel {
    var records: [IntakeRecord] = []
    var status = "Ready to import a PDF"
    var progress: Double?
    private var inbox: SharedInbox?

    init() {
        inbox = try? SharedInbox()
    }

    func reload() {
        records = (try? inbox?.records()) ?? []
    }

    func accept(_ url: URL, source: String) {
        // The marker is only used by the macOS CI Apple-event smoke test. It is
        // written before sandboxed ingestion so delivery is distinguishable
        // from App Group provisioning on an ad-hoc-signed CI build.
        if source == "Open" {
            try? url.path.write(to: URL(fileURLWithPath: "/tmp/tpe-open-smoke"), atomically: true, encoding: .utf8)
        }
        guard let inbox else {
            status = "The shared container is unavailable"
            NSSound.beep()
            return
        }
        progress = 0.1
        do {
            let record = try inbox.importPDF(at: url)
            progress = 1
            reload()
            status = "Imported \(record.filename) from \(source)"
            NSAccessibility.post(element: NSApplication.shared, notification: .announcementRequested, userInfo: [
                .announcement: status,
                .priority: NSAccessibilityPriorityLevel.high.rawValue,
            ])
        } catch {
            progress = nil
            status = "Could not import \(url.lastPathComponent): \(error.localizedDescription)"
            NSSound.beep()
        }
    }

    func showOpenPanel() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.pdf]
        panel.allowsMultipleSelection = true
        if panel.runModal() == .OK {
            for url in panel.urls { accept(url, source: "Open") }
        }
    }
}
