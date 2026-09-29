#if os(macOS)
    import AppKit
    import ShareIntake

    /// Entry point for the Xcode Share Extension target. The extension only stages input.
    open class ShareViewController: NSViewController {
        open var appGroupIdentifier = "group.com.example.text-processing-engine"
        open var hostURL = URL(string: "text-processing-engine://import-shared-items")!
        open var hostNotification = Notification.Name("com.example.text-processing-engine.intake")

        public override func loadView() {
            let label = NSTextField(labelWithString: "Preparing PDFs…")
            label.alignment = .center
            view = NSView(frame: NSRect(x: 0, y: 0, width: 420, height: 120))
            label.frame = view.bounds.insetBy(dx: 20, dy: 20)
            label.autoresizingMask = [.width, .height]
            view.addSubview(label)
        }

        public override func viewDidAppear() {
            super.viewDidAppear()
            Task { await performShare() }
        }

        @MainActor private func performShare() async {
            do {
                guard let items = extensionContext?.inputItems as? [NSExtensionItem] else {
                    throw IntakeError.unsupportedInput("The shared item")
                }
                let providers = items.flatMap { $0.attachments ?? [] }.map(ItemProviderAdapter.init)
                let source = items.compactMap { $0.userInfo?[.init("NSExtensionItemSourceApplication")] as? String }.first
                let stager = try IntakeStager(
                    directories: IntakeDirectories(appGroupIdentifier: appGroupIdentifier)
                )
                _ = try await stager.stage(
                    providers: providers,
                    sourceApplication: source,
                    notifier: DistributedHostNotifier(notificationName: hostNotification)
                )
                // Wakes the host when it is not already observing the distributed notification.
                extensionContext?.open(hostURL) { [weak self] _ in
                    self?.extensionContext?.completeRequest(returningItems: nil)
                }
            } catch {
                presentError(error)
                extensionContext?.cancelRequest(withError: error)
            }
        }
    }
#endif
