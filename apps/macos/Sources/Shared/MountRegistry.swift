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

    func locate(_ url: URL, in records: [MountRecord]? = nil) -> LocatedPath? {
        let path = canonicalFileURL(url).pathComponents
        return (records ?? self.records())
            .compactMap { record -> (MountRecord, [String])? in
                let root = canonicalFileURL(URL(fileURLWithPath: record.mountpoint)).pathComponents
                guard path.starts(with: root) else { return nil }
                return (record, Array(path.dropFirst(root.count)))
            }
            .min { $0.1.count < $1.1.count }
            .map { LocatedPath(mount: $0.0, relativePath: $0.1.joined(separator: "/")) }
    }
}
