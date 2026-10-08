import AppKit
import SwiftUI

enum ActionWindows {
    private static var windows: [NSWindowController] = []

    @MainActor
    static func history(_ context: LocatedPath) {
        present(HistoryView(context: context), title: "Version History", size: NSSize(width: 600, height: 360))
    }

    @MainActor
    static func share(_ context: LocatedPath) {
        present(ShareView(context: context), title: "Share", size: NSSize(width: 560, height: 360))
    }

    @MainActor
    private static func present<Content: View>(_ content: Content, title: String, size: NSSize) {
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: size),
            styleMask: [.titled, .closable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = title
        window.contentView = NSHostingView(rootView: content)
        window.center()
        let controller = NSWindowController(window: window)
        windows.removeAll { $0.window?.isVisible == false }
        windows.append(controller)
        controller.showWindow(nil)
        NSApplication.shared.activate()
    }
}

@MainActor
private final class HistoryModel: ObservableObject {
    @Published var versions: [Version] = []
    @Published var selection: Version.ID?
    @Published var error: String?
    let context: LocatedPath

    init(context: LocatedPath) { self.context = context }

    func load() async {
        do {
            let command = try CLIArguments(
                "history",
                options: context.mount.authorityOptions,
                positionals: [context.mount.space, context.relativePath],
                spaceID: context.mount.space
            )
            versions = try await CLI.json([Version].self, command)
            selection = versions.last?.id
        } catch { self.error = error.localizedDescription }
    }

    func restore() async {
        guard let selection else { return }
        do {
            let command = try CLIArguments(
                "restore",
                options: context.mount.authorityOptions + ["--version", String(selection)],
                positionals: [context.mount.space, context.relativePath],
                spaceID: context.mount.space
            )
            _ = try await CLI.json(Version.self, command)
        } catch { self.error = error.localizedDescription }
    }

    func saveCopy() async {
        guard let selection else { return }
        let panel = NSSavePanel()
        panel.nameFieldStringValue = URL(fileURLWithPath: context.relativePath).lastPathComponent
        guard panel.runModal() == .OK, let output = panel.url else { return }
        do {
            let command = try CLIArguments(
                "show",
                options: context.mount.authorityOptions + [
                    "--version", String(selection), "--output", output.path,
                ],
                positionals: [context.mount.space, context.relativePath],
                spaceID: context.mount.space
            )
            _ = try await CLI.run(command, json: true)
        } catch { self.error = error.localizedDescription }
    }
}

private struct HistoryView: View {
    @StateObject private var model: HistoryModel

    init(context: LocatedPath) {
        _model = StateObject(wrappedValue: HistoryModel(context: context))
    }

    var body: some View {
        VStack(alignment: .leading) {
            List(model.versions, selection: $model.selection) { version in
                HStack {
                    Text("v\(version.id)").monospacedDigit()
                    Text(Date(timeIntervalSince1970: Double(version.createdMs) / 1000), style: .date)
                    Text(version.author).lineLimit(1)
                    Spacer()
                    Text(ByteCountFormatter.string(fromByteCount: Int64(clamping: version.size), countStyle: .file))
                }
                .tag(version.id)
            }
            if let error = model.error { Text(error).foregroundStyle(.red).font(.caption) }
            HStack {
                Spacer()
                Button("Save a Copy…") { Task { await model.saveCopy() } }.disabled(model.selection == nil)
                Button("Restore") { Task { await model.restore() } }.disabled(model.selection == nil)
            }
        }
        .padding()
        .task { await model.load() }
    }
}
