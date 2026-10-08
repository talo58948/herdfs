import SwiftUI

@MainActor
private final class ShareModel: ObservableObject {
    @Published var grants: [AccessGrant] = []
    @Published var selection: AccessGrant.ID?
    @Published var principal = ""
    @Published var permission = "read"
    @Published var error: String?
    let context: LocatedPath

    init(context: LocatedPath) { self.context = context }

    func load() async {
        do {
            let command = try CLIArguments(
                "access",
                options: context.mount.authorityOptions,
                positionals: [context.mount.space, context.relativePath],
                spaceID: context.mount.space
            )
            grants = try await CLI.json([AccessGrant].self, command)
            selection = grants.last?.id
        } catch { self.error = error.localizedDescription }
    }

    func grant() async {
        guard !principal.isEmpty else { return }
        await change("share", [context.mount.space, context.relativePath, principal, permission])
    }

    func revoke() async {
        guard let grant = grants.first(where: { $0.id == selection }) else { return }
        await change("revoke", [context.mount.space, grant.path, grant.principal])
    }

    private func change(_ subcommand: String, _ positionals: [String]) async {
        do {
            let command = try CLIArguments(
                subcommand,
                options: context.mount.authorityOptions,
                positionals: positionals,
                spaceID: context.mount.space
            )
            _ = try await CLI.run(command, json: true)
            principal = ""
            await load()
        } catch { self.error = error.localizedDescription }
    }
}

struct ShareView: View {
    @StateObject private var model: ShareModel

    init(context: LocatedPath) {
        _model = StateObject(wrappedValue: ShareModel(context: context))
    }

    var body: some View {
        VStack(alignment: .leading) {
            List(model.grants, selection: $model.selection) { grant in
                HStack {
                    Text(grant.principal)
                    Spacer()
                    Text(grant.perm).foregroundStyle(.secondary)
                    Text(grant.path).foregroundStyle(.secondary)
                }
                .tag(grant.id)
            }
            HStack {
                TextField("user:name or group:name", text: $model.principal)
                Picker("Permission", selection: $model.permission) {
                    Text("Read").tag("read")
                    Text("Write").tag("write")
                    Text("Admin").tag("admin")
                }
                .frame(width: 150)
                Button("Grant") { Task { await model.grant() } }
            }
            if let error = model.error { Text(error).foregroundStyle(.red).font(.caption) }
            HStack {
                Spacer()
                Button("Revoke Selected") { Task { await model.revoke() } }
                    .disabled(model.selection == nil)
            }
        }
        .padding()
        .task { await model.load() }
    }
}
