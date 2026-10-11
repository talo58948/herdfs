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
    static let container = realHomeDirectory()
        .appending(path: "Library/Containers/dev.herdfs.HerdFS.Finder/Data", directoryHint: .isDirectory)
    static let url = container
        .appending(path: "Library/Application Support/HerdFS/state.json")

    let token: String
    let mounts: [FinderMountState]

    static func read() -> FinderState {
        guard let data = try? Data(contentsOf: url),
              let state = try? JSONDecoder().decode(FinderState.self, from: data) else {
            return FinderState(token: "", mounts: [])
        }
        return state
    }

    func write() throws {
        try prepareFinderStateDirectory(Self.url.deletingLastPathComponent(), inside: Self.container)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(self)
        if let current = try? Data(contentsOf: Self.url), current == data { return }
        try data.write(to: Self.url, options: .atomic)
    }

    func locate(_ url: URL) -> LocatedFinderPath? {
        let path = canonicalFileURL(url).pathComponents
        return mounts.compactMap { mount in
            let rootParts = canonicalFileURL(URL(fileURLWithPath: mount.mountpoint, isDirectory: true)).pathComponents
            guard path.starts(with: rootParts) else { return nil }
            let relative = path.dropFirst(rootParts.count).joined(separator: "/")
            return LocatedFinderPath(mount: mount, relativePath: relative)
        }.min { $0.relativePath.count < $1.relativePath.count }
    }
}

func constantTimeEqual(_ left: String, _ right: String) -> Bool {
    let left = Array(left.utf8)
    let right = Array(right.utf8)
    var difference = left.count ^ right.count
    for index in 0..<max(left.count, right.count) {
        difference |= Int((index < left.count ? left[index] : 0) ^ (index < right.count ? right[index] : 0))
    }
    return difference == 0
}

func prepareFinderStateDirectory(_ directory: URL, inside container: URL) throws {
    let declaredRoot = container.standardizedFileURL.pathComponents
    let target = directory.standardizedFileURL
    guard target.pathComponents.starts(with: declaredRoot) else {
        throw CocoaError(.fileWriteInvalidFileName)
    }
    var isDirectory: ObjCBool = false
    let values = try container.resourceValues(forKeys: [.isSymbolicLinkKey])
    guard FileManager.default.fileExists(atPath: container.path, isDirectory: &isDirectory),
          isDirectory.boolValue, values.isSymbolicLink != true else {
        throw CocoaError(.fileWriteInvalidFileName)
    }
    var current = canonicalFileURL(container)
    guard current.pathComponents == declaredRoot else {
        throw CocoaError(.fileWriteInvalidFileName)
    }
    let root = current.pathComponents
    for component in target.pathComponents.dropFirst(declaredRoot.count) {
        current.append(path: component, directoryHint: .isDirectory)
        isDirectory = false
        if FileManager.default.fileExists(atPath: current.path, isDirectory: &isDirectory) {
            let values = try current.resourceValues(forKeys: [.isSymbolicLinkKey])
            guard isDirectory.boolValue, values.isSymbolicLink != true else {
                throw CocoaError(.fileWriteInvalidFileName)
            }
        } else {
            try FileManager.default.createDirectory(at: current, withIntermediateDirectories: false)
        }
        current = canonicalFileURL(current)
        guard current.pathComponents.starts(with: root) else {
            throw CocoaError(.fileWriteInvalidFileName)
        }
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
