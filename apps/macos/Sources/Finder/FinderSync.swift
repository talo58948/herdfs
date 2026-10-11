import AppKit
import FinderSync
import OSLog

@MainActor
final class FinderSync: FIFinderSync, @unchecked Sendable {
    private var state = FinderState(token: "", mounts: [])
    private var directories: Set<URL> = []
    private var requested: Set<URL> = []
    private var stateTimer: DispatchSourceTimer?
    private let log = Logger(subsystem: "dev.herdfs.HerdFS", category: "FinderSync")

    override nonisolated init() {
        super.init()
        if Thread.isMainThread {
            MainActor.assumeIsolated { start() }
        } else {
            DispatchQueue.main.sync { start() }
        }
    }

    override nonisolated func requestBadgeIdentifier(for url: URL) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            requested.insert(url)
            updateBadge(for: url)
        }
    }

    override nonisolated func menu(for menuKind: FIMenuKind) -> NSMenu? {
        if Thread.isMainThread {
            return MainActor.assumeIsolated { MenuResult(makeMenu(for: menuKind)) }.value
        }
        return DispatchQueue.main.sync { MenuResult(makeMenu(for: menuKind)) }.value
    }

    private func start() {
        log.info("Finder extension started")
        registerBadges()
        refresh()
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 2, repeating: 2)
        timer.setEventHandler { [weak self] in self?.refresh() }
        timer.resume()
        stateTimer = timer
    }

    private func makeMenu(for menuKind: FIMenuKind) -> NSMenu? {
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

    @objc nonisolated private func keepDownloaded() { openOnMain(action: "keep") }
    @objc nonisolated private func unpin() { openOnMain(action: "unpin") }
    @objc nonisolated private func copyLink() { openOnMain(action: "copylink") }
    @objc nonisolated private func versionHistory() { openOnMain(action: "history") }
    @objc nonisolated private func share() { openOnMain(action: "share") }

    private nonisolated func openOnMain(action: String) {
        // Finder reports the selection only while the action runs.
        let controller = FIFinderSyncController.default()
        guard let url = controller.selectedItemURLs()?.first ?? controller.targetedURL() else { return }
        DispatchQueue.main.async { [weak self] in self?.open(action: action, url: url) }
    }

    private func open(action: String, url: URL) {
        guard let located = state.locate(url) else { return }
        var link = URLComponents()
        link.scheme = "herdfs"
        link.host = located.mount.space
        link.path = "/" + located.relativePath
        link.queryItems = [
            URLQueryItem(name: "action", value: action),
            URLQueryItem(name: "token", value: state.token),
        ]
        if let url = link.url {
            log.info("Opening \(action, privacy: .public) link")
            NSWorkspace.shared.open(url)
        }
    }

    private func refresh() {
        state = FinderState.read()
        let nextDirectories = Set(state.mounts.map {
            canonicalFileURL(URL(fileURLWithPath: $0.mountpoint, isDirectory: true))
        })
        if nextDirectories != directories {
            directories = nextDirectories
            FIFinderSyncController.default().directoryURLs = directories
            log.info("Monitoring \(self.directories.count) live mounts")
        }
        requested.forEach(updateBadge)
    }

    private func updateBadge(for url: URL) {
        guard let located = state.locate(url) else { return setBadge(nil, for: url) }
        setBadge(located.mount.badges[located.relativePath], for: url)
    }

    private func setBadge(_ badge: Badge?, for url: URL) {
        FIFinderSyncController.default().setBadgeIdentifier(badge?.identifier ?? "", for: url)
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
            FIFinderSyncController.default().setBadgeImage(image, label: label, forBadgeIdentifier: badge.identifier)
        }
    }

    private func item(_ title: String, _ action: Selector) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
        item.target = self
        return item
    }
}

private struct MenuResult: @unchecked Sendable {
    let value: NSMenu?

    init(_ value: NSMenu?) {
        self.value = value
    }
}
