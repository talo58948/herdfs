import AppKit

extension AppModel {
    func open(_ url: URL) {
        Task {
            let records = await registry.liveRecords().filter(appMount)
            guard let link = AppLink.resolve(
                url,
                authority: Configuration.authority,
                registry: registry,
                records: records
            ) else { return }
            switch link.action {
            case .keep, .unpin:
                guard let action = link.action else { return }
                do {
                    let path = URL(fileURLWithPath: link.path.mount.mountpoint)
                        .appending(path: link.path.relativePath)
                    try await ControlClient.command(
                        action == .keep ? "pin" : "unpin",
                        path: path,
                        mount: link.path.mount
                    )
                    await refreshMounts()
                } catch {
                    fail(error.localizedDescription)
                }
            case .history: ActionWindows.history(link.path)
            case .share: ActionWindows.share(link.path)
            case .copylink:
                guard let value = link.canonicalURL?.absoluteString else { return }
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(value, forType: .string)
            case nil:
                NSWorkspace.shared.activateFileViewerSelecting([
                    URL(fileURLWithPath: link.path.mount.mountpoint).appending(path: link.path.relativePath),
                ])
            }
        }
    }
}
