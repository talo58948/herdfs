import Darwin
import Foundation

func canonicalFileURL(_ url: URL) -> URL {
    let standardized = url.standardizedFileURL
    if let path = realPath(standardized.path) {
        return URL(fileURLWithPath: path, isDirectory: standardized.hasDirectoryPath)
    }
    let parent = standardized.deletingLastPathComponent()
    if let path = realPath(parent.path) {
        return URL(fileURLWithPath: path, isDirectory: true)
            .appending(path: standardized.lastPathComponent)
    }
    return standardized.resolvingSymlinksInPath()
}

private func realPath(_ path: String) -> String? {
    guard let resolved = realpath(path, nil) else { return nil }
    defer { free(resolved) }
    return String(cString: resolved)
}

struct FinderState: Codable, Equatable, Sendable {
    static let url = realHomeDirectory()
        .appending(path: "Library/Containers/dev.herdfs.HerdFS.Finder/Data/Library/Application Support/HerdFS/state.json")

    let mounts: [FinderMountState]

    static func read() -> FinderState {
        guard let data = try? Data(contentsOf: url),
              let state = try? JSONDecoder().decode(FinderState.self, from: data) else {
            return FinderState(mounts: [])
        }
        return state
    }

    func write() throws {
        try FileManager.default.createDirectory(
            at: Self.url.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try JSONEncoder().encode(self).write(to: Self.url, options: .atomic)
    }

    func locate(_ url: URL) -> LocatedFinderPath? {
        let path = canonicalFileURL(url).pathComponents
        return mounts.compactMap { mount in
            let root = canonicalFileURL(URL(fileURLWithPath: mount.mountpoint, isDirectory: true))
            let rootParts = root.pathComponents
            guard path.starts(with: rootParts) else { return nil }
            let relative = path.dropFirst(rootParts.count).joined(separator: "/")
            return LocatedFinderPath(mount: mount, relativePath: relative)
        }.min { $0.relativePath.count < $1.relativePath.count }
    }
}

struct FinderMountState: Codable, Equatable, Sendable {
    let space: String
    let mountpoint: String
    let badges: [String: Badge]
}

struct LocatedFinderPath: Equatable, Sendable {
    let mount: FinderMountState
    let relativePath: String
}
