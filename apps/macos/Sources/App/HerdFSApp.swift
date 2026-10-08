import AppKit
import SwiftUI

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model = AppModel()

    func applicationDidFinishLaunching(_ notification: Notification) {
        model.start()
    }

    func application(_ application: NSApplication, open urls: [URL]) {
        urls.forEach(model.open)
    }
}

@main
struct HerdFSApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        MenuBarExtra("HerdFS", systemImage: "externaldrive.connected.to.line.below") {
            MenuView(model: delegate.model)
                .frame(width: 380)
                .onAppear(perform: delegate.model.menuOpened)
        }
        .menuBarExtraStyle(.window)
    }
}
