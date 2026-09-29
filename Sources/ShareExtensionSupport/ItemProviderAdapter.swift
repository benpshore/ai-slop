#if os(macOS)
    import AppKit
    import ShareIntake
    import UniformTypeIdentifiers

    public struct ItemProviderAdapter: PDFItemProviding, @unchecked Sendable {
        private let provider: NSItemProvider
        public var suggestedName: String? { provider.suggestedName }

        public init(_ provider: NSItemProvider) { self.provider = provider }

        public func loadPDF() async throws -> ProvidedPDF {
            if provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier),
                let url = try await loadFileURL()
            {
                return .file(url, displayName: provider.suggestedName)
            }
            guard provider.hasItemConformingToTypeIdentifier(UTType.pdf.identifier) else {
                throw IntakeError.unsupportedInput(provider.suggestedName ?? "The shared item")
            }
            return .data(try await loadPDFData(), displayName: provider.suggestedName)
        }

        private func loadFileURL() async throws -> URL? {
            try await withCheckedThrowingContinuation { continuation in
                provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier) { item, error in
                    if let error { continuation.resume(throwing: error); return }
                    if let url = item as? URL { continuation.resume(returning: url); return }
                    if let data = item as? Data,
                        let url = URL(dataRepresentation: data, relativeTo: nil)
                    { continuation.resume(returning: url); return }
                    continuation.resume(returning: nil)
                }
            }
        }

        private func loadPDFData() async throws -> Data {
            try await withCheckedThrowingContinuation { continuation in
                provider.loadDataRepresentation(forTypeIdentifier: UTType.pdf.identifier) {
                    data, error in
                    if let error { continuation.resume(throwing: error) }
                    else if let data { continuation.resume(returning: data) }
                    else { continuation.resume(throwing: IntakeError.providerFailed("No PDF data")) }
                }
            }
        }
    }

    public struct DistributedHostNotifier: HostNotifying {
        public let notificationName: Notification.Name
        public init(notificationName: Notification.Name) { self.notificationName = notificationName }

        public func notifyHost(requests: [UUID]) async {
            DistributedNotificationCenter.default().post(
                name: notificationName,
                object: nil,
                userInfo: ["requestIdentifiers": requests.map(\.uuidString)],
                deliverImmediately: true
            )
        }
    }
#endif
