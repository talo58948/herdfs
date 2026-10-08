import AppKit
import FinderSync

@MainActor
final class FinderSync: FIFinderSync {
    private let registry = MountRegistry()
    private let badges = BadgeCache()
    private var registryTimer: DispatchSourceTimer?

    override init() {
        super.init()
        registerBadges()
        refreshDirectories()
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 2, repeating: 2)
        timer.setEventHandler { [weak self] in self?.refreshDirectories() }
        timer.resume()
        registryTimer = timer
    }

    override func requestBadgeIdentifier(for url: URL) {
        guard let located = registry.locate(url) else {
            FIFinderSyncController.default().setBadgeIdentifier("", for: url)
            return
        }
        if let cached = badges.lookup(url) {
            FIFinderSyncController.default().setBadgeIdentifier(cached.badge?.rawValue ?? "", for: url)
            return
        }
        Task {
            let state = try? await ControlClient.file(url, mount: located.mount)
            let badge = state.flatMap(Badge.choose)
            badges.store(badge, for: url)
            await MainActor.run {
                FIFinderSyncController.default().setBadgeIdentifier(badge?.rawValue ?? "", for: url)
            }
        }
    }

    override func menu(for menuKind: FIMenuKind) -> NSMenu? {
        guard menuKind == .contextualMenuForItems else { return nil }
        let menu = NSMenu(title: "HerdFS")
        menu.addItem(item("Keep Downloaded", #selector(keepDownloaded)))
        menu.addItem(item("Unpin", #selector(unpin)))
        menu.addItem(.separator())
        menu.addItem(item("Copy Link", #selector(copyLink)))
        menu.addItem(item("Version History…", #selector(versionHistory)))
        menu.addItem(item("Share…", #selector(share)))
        return menu
    }

    @objc private func keepDownloaded() { control("pin") }
    @objc private func unpin() { control("unpin") }

    @objc private func copyLink() {
        guard let context = selectedContext() else { return }
        var link = URLComponents()
        link.scheme = "herdfs"
        link.host = context.mount.space
        link.path = "/" + context.relativePath
        guard let value = link.url?.absoluteString else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(value, forType: .string)
    }

    @objc private func versionHistory() {
        guard let context = selectedContext() else { return }
        ActionWindows.history(context)
    }

    @objc private func share() {
        guard let context = selectedContext() else { return }
        ActionWindows.share(context)
    }

    private func control(_ op: String) {
        let urls = FIFinderSyncController.default().selectedItemURLs() ?? []
        for url in urls {
            guard let mount = registry.locate(url)?.mount else { continue }
            Task {
                try? await ControlClient.command(op, path: url, mount: mount)
                badges.remove(url)
                await MainActor.run {
                    FIFinderSyncController.default().setBadgeIdentifier("", for: url)
                    self.requestBadgeIdentifier(for: url)
                }
            }
        }
    }

    private func selectedContext() -> ActionContext? {
        let controller = FIFinderSyncController.default()
        guard let url = controller.selectedItemURLs()?.first ?? controller.targetedURL(),
              let located = registry.locate(url) else { return nil }
        return ActionContext(mount: located.mount, relativePath: located.relativePath)
    }

    private func refreshDirectories() {
        FIFinderSyncController.default().directoryURLs = Set(
            registry.records().map { URL(fileURLWithPath: $0.mountpoint) }
        )
    }

    private func registerBadges() {
        let definitions: [(Badge, String, String)] = [
            (.kept, "arrow.down.circle.fill", "Kept downloaded"),
            (.downloading, "arrow.down.circle", "Downloading"),
            (.pending, "arrow.up.circle", "Waiting to upload"),
            (.error, "exclamationmark.triangle.fill", "Error"),
        ]
        for (badge, symbol, label) in definitions {
            guard let image = NSImage(systemSymbolName: symbol, accessibilityDescription: label) else { continue }
            FIFinderSyncController.default().setBadgeImage(image, label: label, forBadgeIdentifier: badge.rawValue)
        }
    }

    private func item(_ title: String, _ action: Selector) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
        item.target = self
        return item
    }
}

private struct CachedBadge {
    let badge: Badge?
}

private final class BadgeCache {
    private let lock = NSLock()
    private var values: [URL: (Date, Badge?)] = [:]

    func lookup(_ url: URL) -> CachedBadge? {
        lock.lock()
        defer { lock.unlock() }
        guard let (time, badge) = values[url], Date().timeIntervalSince(time) < 1 else {
            values.removeValue(forKey: url)
            return nil
        }
        return CachedBadge(badge: badge)
    }

    func store(_ badge: Badge?, for url: URL) {
        lock.lock()
        values[url] = (Date(), badge)
        lock.unlock()
    }

    func remove(_ url: URL) {
        lock.lock()
        values.removeValue(forKey: url)
        lock.unlock()
    }
}
