import AppKit
import SwiftUI

struct ActionContext {
    let mount: MountRecord
    let relativePath: String
}

enum ActionWindows {
    private static var windows: [NSWindowController] = []

    @MainActor
    static func history(_ context: ActionContext) {
        present(HistoryView(context: context), title: "Version History", size: NSSize(width: 600, height: 360))
    }

    @MainActor
    static func share(_ context: ActionContext) {
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
        NSApplication.shared.activate(ignoringOtherApps: true)
    }
}

@MainActor
private final class HistoryModel: ObservableObject {
    @Published var versions: [Version] = []
    @Published var selection: Version.ID?
    @Published var error: String?
    let context: ActionContext

    init(context: ActionContext) { self.context = context }

    func load() async {
        do {
            versions = try await CLI.json([Version].self, context.mount.authorityArguments([
                "history", context.mount.space, context.relativePath,
            ]))
            selection = versions.last?.id
        } catch { self.error = error.localizedDescription }
    }

    func restore() async {
        guard let selection else { return }
        do {
            _ = try await CLI.run(context.mount.authorityArguments([
                "restore", context.mount.space, context.relativePath,
                "--version", String(selection), "--json",
            ]))
        } catch { self.error = error.localizedDescription }
    }

    func saveCopy() async {
        guard let selection else { return }
        let panel = NSSavePanel()
        panel.nameFieldStringValue = URL(fileURLWithPath: context.relativePath).lastPathComponent
        guard panel.runModal() == .OK, let output = panel.url else { return }
        do {
            _ = try await CLI.run(context.mount.authorityArguments([
                "show", context.mount.space, context.relativePath,
                "--version", String(selection), "--output", output.path, "--json",
            ]))
        } catch { self.error = error.localizedDescription }
    }
}

private struct HistoryView: View {
    @StateObject private var model: HistoryModel

    init(context: ActionContext) {
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
