import Foundation

struct LocatedPath: Equatable, Sendable {
    let mount: MountRecord
    let relativePath: String
}

struct MountRegistry {
    let directory: URL

    init(directory: URL = realHomeDirectory()
        .appending(path: ".config/herdfs/mounts")) {
        self.directory = directory
    }

    func records() -> [MountRecord] {
        let files = (try? FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: nil
        )) ?? []
        return files
            .filter { $0.pathExtension == "json" }
            .compactMap { try? Data(contentsOf: $0) }
            .compactMap { try? JSONDecoder().decode(MountRecord.self, from: $0) }
    }

    func liveRecords() async -> [MountRecord] {
        await withTaskGroup(of: MountRecord?.self) { group in
            for record in records() {
                group.addTask {
                    guard (try? await ControlClient.status(record)) != nil else { return nil }
                    return record
                }
            }
            var live: [MountRecord] = []
            var mountpoints: Set<String> = []
            for await record in group {
                guard let record,
                      mountpoints.insert(Self.canonical(record.mountpoint).path).inserted else { continue }
                live.append(record)
            }
            return live
        }
    }

    func locate(_ url: URL, in records: [MountRecord]? = nil) -> LocatedPath? {
        let path = Self.canonical(url.path).pathComponents
        return (records ?? self.records())
            .compactMap { record -> (MountRecord, [String])? in
                let root = Self.canonical(record.mountpoint).pathComponents
                guard path.starts(with: root) else { return nil }
                return (record, Array(path.dropFirst(root.count)))
            }
            .min { $0.1.count < $1.1.count }
            .map { LocatedPath(mount: $0.0, relativePath: $0.1.joined(separator: "/")) }
    }

    private static func canonical(_ path: String) -> URL {
        let url = URL(fileURLWithPath: path).standardizedFileURL
        return url.deletingLastPathComponent()
            .resolvingSymlinksInPath()
            .appending(path: url.lastPathComponent)
    }
}
