import SwiftUI

@MainActor
private final class ShareModel: ObservableObject {
    @Published var grants: [AccessGrant] = []
    @Published var selection: AccessGrant.ID?
    @Published var principal = ""
    @Published var permission = "read"
    @Published var error: String?
    let context: ActionContext

    init(context: ActionContext) { self.context = context }

    func load() async {
        do {
            grants = try await CLI.json([AccessGrant].self, context.mount.authorityArguments([
                "access", context.mount.space, context.relativePath,
            ]))
            selection = grants.last?.id
        } catch { self.error = error.localizedDescription }
    }

    func grant() async {
        guard !principal.isEmpty else { return }
        await change([
            "share", context.mount.space, context.relativePath, principal, permission, "--json",
        ])
    }

    func revoke() async {
        guard let grant = grants.first(where: { $0.id == selection }) else { return }
        await change([
            "revoke", context.mount.space, context.relativePath, grant.principal, "--json",
        ])
    }

    private func change(_ command: [String]) async {
        do {
            _ = try await CLI.run(context.mount.authorityArguments(command))
            principal = ""
            await load()
        } catch { self.error = error.localizedDescription }
    }
}

struct ShareView: View {
    @StateObject private var model: ShareModel

    init(context: ActionContext) {
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
