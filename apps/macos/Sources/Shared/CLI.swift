import Foundation

enum CLIError: LocalizedError {
    case commandFailed(String)
    case invalidSpace(String)

    var errorDescription: String? {
        switch self {
        case .commandFailed(let message): message
        case .invalidSpace(let space): "Invalid space ID \(space.debugDescription)"
        }
    }
}

struct CLIArguments: Equatable, Sendable {
    let subcommand: String
    let options: [String]
    let positionals: [String]

    init(
        _ subcommand: String,
        options: [String] = [],
        positionals: [String] = [],
        spaceID: String? = nil
    ) throws {
        if let spaceID {
            guard Self.isValid(spaceID: spaceID) else {
                throw CLIError.invalidSpace(spaceID)
            }
        }
        self.subcommand = subcommand
        self.options = options
        self.positionals = positionals
    }

    func rendered(extraOptions: [String] = []) -> [String] {
        [subcommand] + options + extraOptions + ["--"] + positionals
    }

    static func isValid(spaceID: String) -> Bool {
        !spaceID.isEmpty && spaceID != "." && spaceID != ".." && !spaceID.contains("/")
    }
}

enum CLI {
    static var executableURL: URL {
        Bundle.main.bundleURL.appending(path: "Contents/MacOS/herdfs")
    }

    static func json<Value: Decodable & Sendable>(
        _ type: Value.Type,
        _ command: CLIArguments
    ) async throws -> Value {
        let data = try await run(command, json: true)
        return try JSONDecoder().decode(type, from: data)
    }

    static func run(_ command: CLIArguments, json: Bool = false) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                do {
                    continuation.resume(returning: try runBlocking(command.rendered(
                        extraOptions: json ? ["--json"] : []
                    )))
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }

    static func spawn(_ command: CLIArguments, stderr log: URL) throws {
        FileManager.default.createFile(atPath: log.path, contents: nil)
        let errors = try FileHandle(forWritingTo: log)
        try errors.truncate(atOffset: 0)
        let process = Process()
        process.executableURL = executableURL
        process.arguments = command.rendered()
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = errors
        do {
            try process.run()
            try errors.close()
        } catch {
            try? errors.close()
            throw error
        }
    }

    private static func runBlocking(_ arguments: [String]) throws -> Data {
        let process = Process()
        let output = Pipe()
        let errors = Pipe()
        process.executableURL = executableURL
        process.arguments = arguments
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = output
        process.standardError = errors
        try process.run()
        let errorRead = PipeDrain(errors.fileHandleForReading)
        errorRead.start()
        let data = output.fileHandleForReading.readDataToEndOfFile()
        let errorData = errorRead.finish()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            let message = String(data: errorData, encoding: .utf8)?
                .trimmingCharacters(in: .whitespacesAndNewlines)
            throw CLIError.commandFailed(message.flatMap { $0.isEmpty ? nil : $0 }
                ?? "herdfs exited with status \(process.terminationStatus)")
        }
        return data
    }
}

private final class PipeDrain: @unchecked Sendable {
    private let handle: FileHandle
    private let group = DispatchGroup()
    private var data = Data()

    init(_ handle: FileHandle) {
        self.handle = handle
    }

    func start() {
        group.enter()
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            data = handle.readDataToEndOfFile()
            group.leave()
        }
    }

    func finish() -> Data {
        group.wait()
        return data
    }
}

extension MountRecord {
    var authorityOptions: [String] { ["--authority", authority] }
}
