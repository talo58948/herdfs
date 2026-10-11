import Foundation

enum FinderStateWriter {
    // Bounds each mount's snapshot and the work needed to refresh it.
    static let pathLimit = 10_000

    static func write(_ mounts: [(MountRecord, MountStatus)]) async throws {
        var snapshots: [FinderMountState] = []
        for (mount, status) in mounts {
            snapshots.append(await snapshot(mount, status))
        }
        try FinderState(mounts: snapshots).write()
    }

    private static func snapshot(_ mount: MountRecord, _ status: MountStatus) async -> FinderMountState {
        var badges: [String: Badge] = [:]
        for upload in status.pending where badges.count < pathLimit {
            badges[upload.path] = .pending
        }
        for pin in status.kept.pins where badges.count < pathLimit {
            let root = URL(fileURLWithPath: mount.mountpoint, isDirectory: true).appending(path: pin.path)
            let file = try? await ControlClient.file(root, mount: mount)
            let badge: Badge = file?.error == nil ? (pin.ready ? .kept : .downloading) : .error
            for path in paths(below: root, relativeTo: mount.mountpoint) where badges.count < pathLimit {
                if badge == .error || badges[path] == nil { badges[path] = badge }
            }
        }
        return FinderMountState(space: mount.space, mountpoint: mount.mountpoint, badges: badges)
    }

    private static func paths(below root: URL, relativeTo mountpoint: String) -> [String] {
        let base = URL(fileURLWithPath: mountpoint, isDirectory: true).standardizedFileURL
        var urls = [root.standardizedFileURL]
        if let children = FileManager.default.enumerator(
            at: root,
            includingPropertiesForKeys: nil,
            options: [.skipsPackageDescendants]
        ) {
            urls.append(contentsOf: children.compactMap { $0 as? URL }.prefix(pathLimit - 1))
        }
        return urls.compactMap { url in
            let parts = url.standardizedFileURL.pathComponents
            let baseParts = base.pathComponents
            guard parts.starts(with: baseParts) else { return nil }
            return parts.dropFirst(baseParts.count).joined(separator: "/")
        }
    }
}
