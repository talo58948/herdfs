import AppKit
import Darwin
import Foundation
import Security
import ServiceManagement
import UserNotifications

struct SpaceState: Identifiable {
    let access: SpaceAccess
    let mount: MountRecord?
    let status: MountStatus?
    var id: String { access.id }
}

@MainActor
final class AppModel: ObservableObject {
    @Published var authority = Configuration.authority
    @Published var issuer = Configuration.issuer
    @Published var clientID = Configuration.clientID
    @Published private(set) var spaces: [SpaceState] = []
    @Published private(set) var busy = false
    @Published var error: String?
    @Published private(set) var openAtLogin = SMAppService.mainApp.status == .enabled

    private let registry = MountRegistry()
    private var started = false

    func start() {
        guard !started else { return }
        started = true
        Task {
            await refresh()
            await mountMissingAtLaunch()
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 2_000_000_000)
                await refresh()
            }
        }
    }

    func saveConfiguration() {
        authority = authority.trimmingCharacters(in: .whitespacesAndNewlines)
        issuer = issuer.trimmingCharacters(in: .whitespacesAndNewlines)
        clientID = clientID.trimmingCharacters(in: .whitespacesAndNewlines)
        Configuration.authority = authority
        Configuration.issuer = issuer
        Configuration.clientID = clientID
        Task { await refresh() }
    }

    func signIn() {
        guard !issuer.isEmpty, !clientID.isEmpty else {
            fail("Issuer and client ID are required")
            return
        }
        busy = true
        Task {
            do {
                _ = try await CLI.run(["login", "--issuer", issuer, "--client-id", clientID])
                try TokenKeychain.storeRefreshTokenCopy()
                await refresh()
            } catch {
                fail(error.localizedDescription)
            }
            busy = false
        }
    }

    func refresh() async {
        guard !authority.isEmpty else { return }
        do {
            let access = try await CLI.json([SpaceAccess].self, ["spaces", "--authority", authority])
            let mounts = registry.records()
            var next: [SpaceState] = []
            for space in access {
                let mount = mounts.first { $0.space == space.space && $0.authority == authority }
                let status = if let mount { try? await ControlClient.status(mount) } else { nil }
                next.append(SpaceState(access: space, mount: mount, status: status))
            }
            spaces = next
            error = nil
        } catch {
            fail(error.localizedDescription)
        }
    }

    func mount(_ space: SpaceState) {
        let home = FileManager.default.homeDirectoryForCurrentUser
        let mountpoint = home.appending(path: "HerdFS/\(space.id)")
        let state = home.appending(path: ".config/herdfs/state/\(space.id)")
        do {
            try FileManager.default.createDirectory(at: mountpoint, withIntermediateDirectories: true)
            try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true)
            try CLI.spawn([
                "mount", "--nfs", "--authority", authority, "--space", space.id,
                "--cache", state.appending(path: "cache").path,
                "--cache-bytes", "10737418240",
                "--journal", state.appending(path: "journal").path,
                mountpoint.path,
            ])
            Task {
                try? await Task.sleep(nanoseconds: 700_000_000)
                await refresh()
            }
        } catch {
            fail(error.localizedDescription)
        }
    }

    func unmount(_ space: SpaceState) {
        guard let pid = space.mount?.pid, kill(pid, SIGTERM) == 0 else {
            fail("Could not stop the mount")
            return
        }
        Task {
            try? await Task.sleep(nanoseconds: 700_000_000)
            await refresh()
        }
    }

    func setPaused(_ paused: Bool, for space: SpaceState) {
        guard let mount = space.mount else { return }
        Task {
            do {
                try await ControlClient.command(paused ? "pause" : "resume", mount: mount)
                await refresh()
            } catch {
                fail(error.localizedDescription)
            }
        }
    }

    func setOpenAtLogin(_ enabled: Bool) {
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            openAtLogin = SMAppService.mainApp.status == .enabled
        } catch {
            fail(error.localizedDescription)
        }
    }

    func open(_ link: URL) {
        guard link.scheme == "herdfs", let space = link.host,
              let mount = registry.records().first(where: { $0.space == space }) else { return }
        let file = link.pathComponents.dropFirst().reduce(URL(fileURLWithPath: mount.mountpoint)) {
            $0.appending(path: $1)
        }
        NSWorkspace.shared.activateFileViewerSelecting([file])
    }

    private func mountMissingAtLaunch() async {
        for space in spaces where space.mount == nil { mount(space) }
    }

    private func fail(_ message: String) {
        guard error != message else { return }
        error = message
        ErrorNotification.send(message)
    }
}

private enum TokenKeychain {
    static func storeRefreshTokenCopy() throws {
        let token = FileManager.default.homeDirectoryForCurrentUser
            .appending(path: ".config/herdfs/token")
        let object = try JSONSerialization.jsonObject(with: Data(contentsOf: token)) as? [String: Any]
        guard let refresh = object?["refresh_token"] as? String else {
            throw CLIError.failed("login token had no refresh token")
        }
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "dev.herdfs.HerdFS",
            kSecAttrAccount as String: "refresh-token",
        ]
        SecItemDelete(base as CFDictionary)
        var item = base
        item[kSecValueData as String] = Data(refresh.utf8)
        let status = SecItemAdd(item as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw CLIError.failed("Keychain returned \(status)")
        }
    }
}

private enum ErrorNotification {
    static func send(_ message: String) {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert]) { allowed, _ in
            guard allowed else { return }
            let content = UNMutableNotificationContent()
            content.title = "HerdFS error"
            content.body = message
            center.add(UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil))
        }
    }
}
