import SwiftUI

struct MenuView: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("HerdFS").font(.headline)
                Spacer()
                ProgressView().controlSize(.small).opacity(model.busy ? 1 : 0)
                Button { Task { await model.refreshSpaces() } } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
            }
            configuration
            Divider()
            if model.spaces.isEmpty {
                Text("No spaces").foregroundStyle(.secondary)
            } else {
                ForEach(model.spaces) { space in
                    SpaceRow(model: model, space: space)
                    if space.id != model.spaces.last?.id { Divider() }
                }
            }
            if let error = model.error {
                Text(error).font(.caption).foregroundStyle(.red).lineLimit(3)
            }
            Divider()
            Toggle("Open at login", isOn: Binding(
                get: { model.openAtLogin },
                set: model.setOpenAtLogin
            ))
            if model.loginRequiresApproval {
                Button("Approve Open at Login…", action: model.showLoginItemSettings)
            }
            if !model.finderEnabled {
                HStack {
                    Text("Finder extension is disabled").foregroundStyle(.secondary)
                    Spacer()
                    Button("Enable…", action: model.showFinderSettings)
                }
            }
            HStack {
                Button("Sign in…", action: model.signIn).disabled(model.busy)
                Spacer()
                Button("Quit") { NSApplication.shared.terminate(nil) }
            }
        }
        .padding(14)
    }

    private var configuration: some View {
        Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 8, verticalSpacing: 6) {
            GridRow {
                Text("Authority")
                TextField("https://authority.example", text: $model.authority)
            }
            GridRow {
                Text("Issuer")
                TextField("https://issuer.example", text: $model.issuer)
            }
            GridRow {
                Text("Client ID")
                TextField("herdfs", text: $model.clientID)
            }
            GridRow {
                HStack {
                    Spacer()
                    Button("Save settings", action: model.saveConfiguration)
                        .controlSize(.small)
                }
                .gridCellColumns(2)
            }
        }
        .textFieldStyle(.roundedBorder)
    }
}

private struct SpaceRow: View {
    let model: AppModel
    let space: SpaceState

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack {
                Text(space.id).fontWeight(.medium)
                Spacer()
                if space.mount == nil {
                    Button("Mount") { model.mount(space) }
                } else {
                    Button("Unmount") { model.unmount(space) }
                }
            }
            if let status = space.status {
                HStack {
                    Label(status.connected ? "Connected" : "Offline",
                          systemImage: status.connected ? "network" : "network.slash")
                    Spacer()
                    Text("\(status.pending.count) pending")
                    Button(status.paused ? "Resume" : "Pause") {
                        model.setPaused(!status.paused, for: space)
                    }
                    .controlSize(.small)
                }
                Text("Cache \(bytes(status.cache.used)) / \(bytes(status.cache.budget)) · kept \(bytes(status.kept.bytes))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                ForEach(status.pending.prefix(3), id: \.path) { upload in
                    Text("↑ \(upload.path) · \(bytes(upload.bytes))")
                        .font(.caption)
                        .lineLimit(1)
                }
            } else if space.mount != nil {
                Text("Mount starting…").font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func bytes(_ count: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: count), countStyle: .file)
    }
}
