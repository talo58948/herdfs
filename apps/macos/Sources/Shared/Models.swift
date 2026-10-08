import Foundation

struct MountRecord: Codable, Equatable, Identifiable {
    let id: String
    let space: String
    let authority: String
    let mountpoint: String
    let socket: String
    let pid: Int32
}

struct PendingUpload: Codable, Equatable {
    let path: String
    let bytes: UInt64
}

struct CacheUse: Codable, Equatable {
    let used: UInt64
    let budget: UInt64
}

struct KeptPath: Codable, Equatable {
    let path: String
    let ready: Bool
}

struct KeptUse: Codable, Equatable {
    let bytes: UInt64
    let pins: [KeptPath]
}

struct MountStatus: Codable, Equatable {
    let ok: Bool
    let space: String
    let mountpoint: String
    let connected: Bool
    let roundTripMs: UInt64
    let paused: Bool
    let pending: [PendingUpload]
    let cache: CacheUse
    let kept: KeptUse

    enum CodingKeys: String, CodingKey {
        case ok, space, mountpoint, connected, paused, pending, cache, kept
        case roundTripMs = "round_trip_ms"
    }
}

struct FileState: Codable, Equatable {
    let ok: Bool
    let kept: Bool
    let downloading: Bool
    let pending: Bool
    let error: String?
}

struct ControlError: Codable, LocalizedError, Equatable {
    let ok: Bool
    let error: String

    var errorDescription: String? { error }
}

struct ControlOK: Codable, Equatable {
    let ok: Bool
}

struct SpaceAccess: Codable, Equatable, Identifiable {
    var id: String { space }
    let space: String
    let rootPerm: String?

    enum CodingKeys: String, CodingKey {
        case space
        case rootPerm = "root_perm"
    }
}

struct Version: Codable, Equatable, Identifiable {
    let id: UInt64
    let entry: UInt64
    let size: UInt64
    let author: String
    let createdMs: Int64

    enum CodingKeys: String, CodingKey {
        case id, entry, size, author
        case createdMs = "created_ms"
    }
}

struct AccessGrant: Codable, Equatable, Identifiable {
    var id: String { "\(path)\u{0}\(principal)" }
    let path: String
    let principal: String
    let perm: String
}

enum Badge: String, Equatable {
    case kept = "kept"
    case downloading = "downloading"
    case pending = "pending"
    case error = "error"

    static func choose(_ state: FileState) -> Badge? {
        if state.error != nil { return .error }
        if state.pending { return .pending }
        if state.downloading { return .downloading }
        if state.kept { return .kept }
        return nil
    }
}
