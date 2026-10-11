import Foundation

enum FinderStateWriter {
    // Bounds each mount's snapshot and the work needed to refresh it.
    static let pathLimit = 10_000

    static func write(_ mounts: [(MountRecord, MountStatus)], token: String) async throws {
        var snapshots: [FinderMountState] = []
        for (mount, status) in mounts {
            snapshots.append(await snapshot(mount, status))
        }
        try FinderState(token: token, mounts: snapshots).write()
    }

    private static func snapshot(_ mount: MountRecord, _ status: MountStatus) async -> FinderMountState {
        let root = canonicalFileURL(URL(fileURLWithPath: mount.mountpoint, isDirectory: true))
        var badges: [String: Badge] = [:]
        for upload in status.pending.prefix(pathLimit) {
            guard badges.count < pathLimit else { break }
            if let path = relativePath(root.appending(path: upload.path), to: root) {
                badges[path] = .pending
            }
        }
        for pin in (status.kept.pins ?? []).prefix(pathLimit) {
            guard badges.count < pathLimit else { break }
            let pinned = root.appending(path: pin.path)
            let badge: Badge
            if let file = try? await ControlClient.file(pinned, mount: mount) {
                badge = Badge.choose(file) ?? (pin.ready ? .kept : .downloading)
            } else {
                badge = .error
            }
            for path in paths(below: pinned, relativeTo: root, limit: pathLimit - badges.count) {
                if badge == .error || badges[path] == nil { badges[path] = badge }
            }
        }
        return FinderMountState(space: mount.space, mountpoint: root.path, badges: badges)
    }

    private static func paths(below root: URL, relativeTo base: URL, limit: Int) -> [String] {
        let baseParts = base.pathComponents
        var paths: [String] = []
        let append = { (url: URL) in
            let parts = url.standardizedFileURL.pathComponents
            guard parts.starts(with: baseParts) else { return }
            paths.append(parts.dropFirst(baseParts.count).joined(separator: "/"))
        }
        append(root)
        if let children = FileManager.default.enumerator(
            at: root,
            includingPropertiesForKeys: nil,
            options: [.skipsPackageDescendants]
        ) {
            for case let child as URL in children {
                if paths.count >= limit { break }
                append(child)
            }
        }
        return Array(paths.prefix(limit))
    }

    private static func relativePath(_ url: URL, to base: URL) -> String? {
        let parts = canonicalFileURL(url).pathComponents
        let baseParts = canonicalFileURL(base).pathComponents
        guard parts.starts(with: baseParts) else { return nil }
        return parts.dropFirst(baseParts.count).joined(separator: "/")
    }
}
