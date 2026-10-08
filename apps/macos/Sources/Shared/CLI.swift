import Foundation

enum CLIError: LocalizedError {
    case failed(String)

    var errorDescription: String? {
        switch self {
        case .failed(let message): message
        }
    }
}

enum CLI {
    static var executableURL: URL {
        let bundle = Bundle.main.bundleURL
        if bundle.pathExtension == "appex" {
            return bundle.deletingLastPathComponent()
                .deletingLastPathComponent()
                .appending(path: "MacOS/herdfs")
        }
        return bundle.appending(path: "Contents/MacOS/herdfs")
    }

    static func json<Value: Decodable>(_ type: Value.Type, _ arguments: [String]) async throws -> Value {
        let data = try await run(arguments + ["--json"])
        return try JSONDecoder().decode(type, from: data)
    }

    static func run(_ arguments: [String]) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                do {
                    continuation.resume(returning: try runBlocking(arguments))
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }

    static func spawn(_ arguments: [String]) throws {
        let process = Process()
        process.executableURL = executableURL
        process.arguments = arguments
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
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
        let data = output.fileHandleForReading.readDataToEndOfFile()
        let errorData = errors.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            let message = String(data: errorData, encoding: .utf8)?
                .trimmingCharacters(in: .whitespacesAndNewlines)
            throw CLIError.failed(message.flatMap { $0.isEmpty ? nil : $0 }
                ?? "herdfs exited with status \(process.terminationStatus)")
        }
        return data
    }
}

extension MountRecord {
    func authorityArguments(_ command: [String]) -> [String] {
        command + ["--authority", authority]
    }
}
