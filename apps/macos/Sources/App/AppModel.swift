import Darwin
import FinderSync
import Foundation
import ServiceManagement
import UserNotifications

struct SpaceState: Identifiable, Equatable, Sendable {
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
    @Published private(set) var finderEnabled = FIFinderSyncController.isExtensionEnabled
    @Published private(set) var openAtLogin = SMAppService.mainApp.status == .enabled
    @Published private(set) var loginRequiresApproval = SMAppService.mainApp.status == .requiresApproval

    private static let cacheBytes = "10737418240"
    private static let processPathBufferSize = 4_096
    let registry = MountRegistry()
    let finderToken = UUID().uuidString
    private var started = false

    func start() {
        guard !started else { return }
        started = true
        Task {
            await refreshSpaces()
            mountMissingAtLaunch()
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(60))
                await refreshSpaces()
            }
        }
        Task {
            while !Task.isCancelled {
                await refreshMounts()
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    func menuOpened() {
        updateServiceStates()
        Task { await refreshSpaces() }
    }

    func saveConfiguration() {
        authority = authority.trimmingCharacters(in: .whitespacesAndNewlines)
        issuer = issuer.trimmingCharacters(in: .whitespacesAndNewlines)
        clientID = clientID.trimmingCharacters(in: .whitespacesAndNewlines)
        Configuration.authority = authority
        Configuration.issuer = issuer
        Configuration.clientID = clientID
        Task { await refreshSpaces() }
    }

    func signIn() {
        guard !issuer.isEmpty, !clientID.isEmpty else {
            fail("Issuer and client ID are required")
            return
        }
        busy = true
        Task {
            do {
                let command = try CLIArguments("login", options: [
                    "--issuer", issuer, "--client-id", clientID,
                ])
                _ = try await CLI.run(command)
                await refreshSpaces()
            } catch {
                fail(error.localizedDescription)
            }
            busy = false
        }
    }

    func refreshSpaces() async {
        let savedAuthority = Configuration.authority
        guard !savedAuthority.isEmpty else { return }
        do {
            let command = try CLIArguments("spaces", options: ["--authority", savedAuthority])
            let access = try await CLI.json([SpaceAccess].self, command)
            let current = Dictionary(uniqueKeysWithValues: spaces.map { ($0.id, $0) })
            let next = access.map { item in
                let previous = current[item.id]
                return SpaceState(access: item, mount: previous?.mount, status: previous?.status)
            }
            if next != spaces { spaces = next }
            error = nil
            await refreshMounts()
        } catch {
            fail(error.localizedDescription)
        }
    }

    func mount(_ space: SpaceState) {
        let home = realHomeDirectory()
        let mountpoint = expectedMountpoint(space.id)
        let state = home.appending(path: ".config/herdfs/state/\(space.id)")
        let log = state.appending(path: "mount.log")
        do {
            try FileManager.default.createDirectory(at: mountpoint, withIntermediateDirectories: true)
            try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true)
            let command = try CLIArguments(
                "mount",
                options: [
                    "--nfs", "--authority", Configuration.authority, "--space", space.id,
                    "--cache", state.appending(path: "cache").path,
                    "--cache-bytes", Self.cacheBytes,
                    "--journal", state.appending(path: "journal").path,
                ],
                positionals: [mountpoint.path],
                spaceID: space.id
            )
            try CLI.spawn(command, stderr: log)
            Task {
                try? await Task.sleep(for: .seconds(1))
                await refreshMounts()
                surfaceMountFailure(spaceID: space.id, log: log)
            }
        } catch {
            fail(error.localizedDescription)
        }
    }

    func unmount(_ space: SpaceState) {
        let executable = CLI.executableURL.standardizedFileURL.path
        guard let mount = space.mount, processPath(mount.pid) == executable,
              kill(mount.pid, SIGTERM) == 0 else {
            fail("Could not verify and stop the mount process")
            return
        }
        Task {
            try? await Task.sleep(for: .seconds(1))
            await refreshMounts()
        }
    }

    func setPaused(_ paused: Bool, for space: SpaceState) {
        guard let mount = space.mount else { return }
        Task {
            do {
                try await ControlClient.command(paused ? "pause" : "resume", mount: mount)
                await refreshMounts()
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
            updateServiceStates()
        } catch {
            fail(error.localizedDescription)
        }
    }

    func showFinderSettings() {
        FIFinderSyncController.showExtensionManagementInterface()
    }

    func showLoginItemSettings() {
        SMAppService.openSystemSettingsLoginItems()
    }

    func refreshMounts() async {
        let authority = Configuration.authority
        let records = registry.records().filter {
            $0.authority == authority && appMount($0)
        }
        await withTaskGroup(of: (String, MountRecord, MountStatus?).self) { group in
            for record in records where spaces.contains(where: { $0.id == record.space }) {
                group.addTask {
                    (record.space, record, try? await ControlClient.status(record))
                }
            }
            var mounted: [String: (MountRecord, MountStatus)] = [:]
            for await (space, record, status) in group {
                if let status { mounted[space] = (record, status) }
            }
            let next = spaces.map { space in
                guard let (mount, status) = mounted[space.id] else {
                    return SpaceState(access: space.access, mount: nil, status: nil)
                }
                return SpaceState(access: space.access, mount: mount, status: status)
            }
            if next != spaces { spaces = next }
            do {
                try await FinderStateWriter.write(Array(mounted.values), token: finderToken)
            } catch {
                fail("Could not update Finder badges: \(error.localizedDescription)")
            }
        }
    }

    private func mountMissingAtLaunch() {
        for space in spaces where space.mount == nil { mount(space) }
    }

    private func surfaceMountFailure(spaceID: String, log: URL) {
        let found = registry.records().contains {
            $0.space == spaceID && $0.authority == Configuration.authority && appMount($0)
        }
        guard !found else { return }
        let data = (try? Data(contentsOf: log)) ?? Data()
        let stderr = String(decoding: data.suffix(2_000), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        fail(stderr.isEmpty ? "Mount did not register; see \(log.path)" : stderr)
    }

    func appMount(_ record: MountRecord) -> Bool {
        URL(fileURLWithPath: record.mountpoint).resolvingSymlinksInPath()
            == expectedMountpoint(record.space).resolvingSymlinksInPath()
            && processPath(record.pid) == CLI.executableURL.standardizedFileURL.path
    }

    private func expectedMountpoint(_ space: String) -> URL {
        realHomeDirectory().appending(path: "HerdFS/\(space)")
    }

    private func processPath(_ pid: Int32) -> String? {
        var buffer = [CChar](repeating: 0, count: Self.processPathBufferSize)
        guard proc_pidpath(pid, &buffer, UInt32(buffer.count)) > 0 else { return nil }
        let end = buffer.firstIndex(of: 0) ?? buffer.endIndex
        let path = String(decoding: buffer[..<end].map { UInt8(bitPattern: $0) }, as: UTF8.self)
        return URL(fileURLWithPath: path).standardizedFileURL.path
    }

    private func updateServiceStates() {
        finderEnabled = FIFinderSyncController.isExtensionEnabled
        let status = SMAppService.mainApp.status
        openAtLogin = status == .enabled
        loginRequiresApproval = status == .requiresApproval
    }

    func fail(_ message: String) {
        guard error != message else { return }
        error = message
        ErrorNotification.send(message)
    }
}

private enum ErrorNotification {
    static func send(_ message: String) {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert]) { allowed, _ in
            guard allowed else { return }
            let content = UNMutableNotificationContent()
            content.title = "HerdFS error"
            content.body = message
            UNUserNotificationCenter.current().add(
                UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
            )
        }
    }
}
