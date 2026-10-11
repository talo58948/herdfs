import Foundation

struct MountRecord: Decodable, Equatable, Identifiable, Sendable {
    var id: String { socket }
    let space: String
    let authority: String
    let mountpoint: String
    let socket: String
    let pid: Int32
}

struct PendingUpload: Decodable, Equatable, Sendable {
    let path: String
    let bytes: UInt64
}

struct CacheUse: Decodable, Equatable, Sendable {
    let used: UInt64
    let budget: UInt64
}

struct KeptUse: Decodable, Equatable, Sendable {
    let bytes: UInt64
    let pins: [KeptPin]?
}

struct KeptPin: Decodable, Equatable, Sendable {
    let path: String
    let ready: Bool
}

struct MountStatus: Decodable, Equatable, Sendable {
    let connected: Bool
    let paused: Bool
    let pending: [PendingUpload]
    let cache: CacheUse
    let kept: KeptUse
}

struct FileState: Decodable, Equatable, Sendable {
    let kept: Bool
    let downloading: Bool
    let pending: Bool
    let error: String?
}

struct ControlError: Decodable, LocalizedError, Equatable, Sendable {
    let ok: Bool
    let error: String

    var errorDescription: String? { error }
}

struct ControlOK: Decodable, Equatable, Sendable {}

struct SpaceAccess: Decodable, Equatable, Identifiable, Sendable {
    var id: String { space }
    let space: String
}

struct Version: Decodable, Equatable, Identifiable, Sendable {
    let id: UInt64
    let size: UInt64
    let author: String
    let createdMs: Int64

    enum CodingKeys: String, CodingKey {
        case id, size, author
        case createdMs = "created_ms"
    }
}

struct AccessGrant: Decodable, Equatable, Identifiable, Sendable {
    var id: String { "\(path)\u{0}\(principal)" }
    let path: String
    let principal: String
    let perm: String
}

enum Badge: String, Codable, Equatable, Sendable {
    case kept
    case downloading
    case pending
    case error

    var identifier: String { rawValue }

    static func choose(_ state: FileState) -> Badge? {
        if state.error != nil { return .error }
        if state.pending { return .pending }
        if state.downloading { return .downloading }
        if state.kept { return .kept }
        return nil
    }
}
