import AppKit
import TPEMacSupport

@objc(ShareViewController)
final class ShareViewController: NSViewController {
    override func loadView() {
        let label = NSTextField(labelWithString: "Importing PDF…")
        label.alignment = .center
        label.setAccessibilityLabel("Share import status")
        label.setAccessibilityValue("Importing PDF")
        view = NSView(frame: NSRect(x: 0, y: 0, width: 360, height: 120))
        label.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(label)
        NSLayoutConstraint.activate([
            label.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            label.centerYAnchor.constraint(equalTo: view.centerYAnchor),
        ])
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        let providers = extensionContext?.inputItems
            .compactMap { $0 as? NSExtensionItem }
            .flatMap { $0.attachments ?? [] } ?? []
        Task { @MainActor in
            do {
                _ = try await ShareImporter.importFirstPDF(from: providers, into: SharedInbox())
                extensionContext?.completeRequest(returningItems: nil)
            } catch {
                extensionContext?.cancelRequest(withError: error)
            }
        }
    }
}
