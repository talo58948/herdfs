import SwiftUI

@main
struct HerdFSApp: App {
    @StateObject private var model = AppModel()

    var body: some Scene {
        MenuBarExtra("HerdFS", systemImage: "externaldrive.connected.to.line.below") {
            MenuView(model: model)
                .frame(width: 380)
                .task { model.start() }
                .onOpenURL { model.open($0) }
        }
        .menuBarExtraStyle(.window)
    }
}
