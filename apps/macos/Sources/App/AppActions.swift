import AppKit

extension AppModel {
    func open(_ url: URL) {
        Task {
            let records = registry.records().filter(appMount)
            guard let link = AppLink.resolve(
                url,
                authority: Configuration.authority,
                token: finderToken,
                registry: registry,
                records: records
            ) else { return }
            switch link.action {
            case .keep: await control("pin", link.path)
            case .unpin: await control("unpin", link.path)
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

    private func control(_ op: String, _ path: LocatedPath) async {
        do {
            let file = URL(fileURLWithPath: path.mount.mountpoint).appending(path: path.relativePath)
            try await ControlClient.command(op, path: file, mount: path.mount)
            await refreshMounts()
        } catch {
            fail(error.localizedDescription)
        }
    }
}
