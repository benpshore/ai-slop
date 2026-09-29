public func greeting() -> String {
    "Hello from text-processing-engine!"
}

#if os(macOS)
import AppKit
import Foundation

/// Persists the least authority needed for a watched folder: one read-only,
/// security-scoped bookmark for the exact folder selected in `NSOpenPanel`.
public struct WatchedFolderBookmarks {
    private let defaults: UserDefaults

    public init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    public func chooseFolder() -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        return panel.runModal() == .OK ? panel.url : nil
    }

    public func save(_ folder: URL, key: String) throws {
        guard folder.isFileURL else { throw CocoaError(.fileNoSuchFile) }
        let data = try folder.bookmarkData(
            options: [.withSecurityScope, .securityScopeAllowOnlyReadAccess],
            includingResourceValuesForKeys: nil,
            relativeTo: nil
        )
        defaults.set(data, forKey: key)
    }

    /// Resolves access for an operation. Stale bookmarks are deliberately not
    /// silently widened; the caller must ask the user to select the folder again.
    public func withAccess<T>(key: String, _ body: (URL) throws -> T) throws -> T {
        guard let data = defaults.data(forKey: key) else {
            throw CocoaError(.fileNoSuchFile)
        }
        var stale = false
        let folder = try URL(
            resolvingBookmarkData: data,
            options: [.withSecurityScope, .withoutUI],
            relativeTo: nil,
            bookmarkDataIsStale: &stale
        )
        guard !stale else { throw CocoaError(.fileReadUnknown) }
        guard folder.startAccessingSecurityScopedResource() else {
            throw CocoaError(.fileReadNoPermission)
        }
        defer { folder.stopAccessingSecurityScopedResource() }
        return try body(folder)
    }
}
#endif
