import AppKit
import FinderSync

@MainActor
final class FinderSync: FIFinderSync, @unchecked Sendable {
    private let registry = MountRegistry()
    private var records: [MountRecord] = []
    private var directories: Set<URL> = []
    private var requested: Set<URL> = []
    private var commandErrors: Set<URL> = []
    private var registryTimer: DispatchSourceTimer?

    override nonisolated init() {
        super.init()
        MainActor.assumeIsolated { start() }
    }

    override nonisolated func requestBadgeIdentifier(for url: URL) {
        MainActor.assumeIsolated {
            requested.insert(url)
            updateBadge(for: url)
        }
    }

    override nonisolated func menu(for menuKind: FIMenuKind) -> NSMenu? {
        MainActor.assumeIsolated { MenuResult(makeMenu(for: menuKind)) }.value
    }

    private func start() {
        registerBadges()
        refresh()
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 2, repeating: 2)
        timer.setEventHandler { [weak self] in self?.refresh() }
        timer.resume()
        registryTimer = timer
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

    @objc private func keepDownloaded() { control("pin") }
    @objc private func unpin() { control("unpin") }
    @objc private func copyLink() { open(action: "copylink") }
    @objc private func versionHistory() { open(action: "history") }
    @objc private func share() { open(action: "share") }

    private func control(_ op: String) {
        let urls = FIFinderSyncController.default().selectedItemURLs() ?? []
        for url in urls {
            guard let mount = registry.locate(url, in: records)?.mount else { continue }
            Task {
                do {
                    try await ControlClient.command(op, path: url, mount: mount)
                    commandErrors.remove(url)
                    updateBadge(for: url)
                } catch {
                    commandErrors.insert(url)
                    setBadge(.error, for: url)
                }
            }
        }
    }

    private func open(action: String) {
        let controller = FIFinderSyncController.default()
        guard let url = controller.selectedItemURLs()?.first ?? controller.targetedURL(),
              let located = registry.locate(url, in: records) else { return }
        var link = URLComponents()
        link.scheme = "herdfs"
        link.host = located.mount.space
        link.path = "/" + located.relativePath
        link.queryItems = [URLQueryItem(name: "action", value: action)]
        if let url = link.url { NSWorkspace.shared.open(url) }
    }

    private func refresh() {
        let next = registry.records()
        records = next
        let nextDirectories = Set(records.map { URL(fileURLWithPath: $0.mountpoint) })
        if nextDirectories != directories {
            directories = nextDirectories
            FIFinderSyncController.default().directoryURLs = directories
        }
        requested.forEach(updateBadge)
    }

    private func updateBadge(for url: URL) {
        guard !commandErrors.contains(url), let mount = registry.locate(url, in: records)?.mount else {
            setBadge(commandErrors.contains(url) ? .error : nil, for: url)
            return
        }
        Task {
            let state = try? await ControlClient.file(url, mount: mount)
            setBadge(state.flatMap(Badge.choose), for: url)
        }
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
