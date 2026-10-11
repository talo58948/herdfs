import Foundation

enum LinkAction: String, Equatable {
    case keep
    case unpin
    case history
    case share
    case copylink
}

struct AppLink: Equatable {
    let path: LocatedPath
    let action: LinkAction?

    static func resolve(
        _ link: URL,
        authority: String,
        registry: MountRegistry,
        records: [MountRecord]
    ) -> AppLink? {
        guard link.scheme == "herdfs", let space = link.host,
              CLIArguments.isValid(spaceID: space),
              let mount = records.first(where: { $0.space == space && $0.authority == authority }),
              let components = URLComponents(url: link, resolvingAgainstBaseURL: false) else { return nil }
        let encoded = components.percentEncodedPath.split(separator: "/")
        let parts = encoded.compactMap {
            String($0).removingPercentEncoding
        }
        guard parts.count == encoded.count,
              !parts.contains(where: { $0 == ".." || $0.contains("/") }) else { return nil }
        let root = URL(fileURLWithPath: mount.mountpoint, isDirectory: true)
        let file = parts.reduce(root) { $0.appending(path: $1) }.standardizedFileURL
        guard let located = registry.locate(file, in: records), located.mount == mount else { return nil }
        let value = components.queryItems?.first(where: { $0.name == "action" })?.value
        let action = value.flatMap(LinkAction.init(rawValue:))
        guard value == nil || action != nil else { return nil }
        return AppLink(path: located, action: action)
    }

    var canonicalURL: URL? {
        var link = URLComponents()
        link.scheme = "herdfs"
        link.host = path.mount.space
        link.path = "/" + path.relativePath
        return link.url
    }
}
