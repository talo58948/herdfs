import Foundation
import Network

enum ControlClient {
    static func status(_ mount: MountRecord) async throws -> MountStatus {
        try await request(["op": "status"], from: mount.socket, as: MountStatus.self)
    }

    static func file(_ url: URL, mount: MountRecord) async throws -> FileState {
        try await request(["op": "file", "path": url.path], from: mount.socket, as: FileState.self)
    }

    static func command(_ op: String, path: URL? = nil, mount: MountRecord) async throws {
        var payload = ["op": op]
        if let path { payload["path"] = path.path }
        _ = try await request(payload, from: mount.socket, as: ControlOK.self)
    }

    private static func request<Value: Decodable>(
        _ payload: [String: String],
        from socket: String,
        as type: Value.Type
    ) async throws -> Value {
        var data = try JSONSerialization.data(withJSONObject: payload)
        data.append(0x0A)
        return try await withCheckedThrowingContinuation { continuation in
            LineRequest(data: data, socket: socket, continuation: continuation).start()
        }
    }
}

private final class LineRequest<Value: Decodable> {
    private let connection: NWConnection
    private let data: Data
    private let continuation: CheckedContinuation<Value, Error>
    private let queue = DispatchQueue(label: "dev.herdfs.HerdFS.control")
    private var received = Data()
    private var finished = false

    init(data: Data, socket: String, continuation: CheckedContinuation<Value, Error>) {
        self.data = data
        self.continuation = continuation
        connection = NWConnection(to: .unix(path: socket), using: .tcp)
    }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready: self.send()
            case .failed(let error): self.finish(.failure(error))
            case .cancelled: self.finish(.failure(CLIError.failed("control socket closed")))
            default: break
            }
        }
        connection.start(queue: queue)
    }

    private func send() {
        connection.send(content: data, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if let error { self.finish(.failure(error)) } else { self.read() }
        })
    }

    private func read() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { [weak self] data, _, complete, error in
            guard let self else { return }
            if let data { self.received.append(data) }
            if let end = self.received.firstIndex(of: 0x0A) {
                self.decode(Data(self.received[..<end]))
            } else if let error {
                self.finish(.failure(error))
            } else if complete {
                self.finish(.failure(CLIError.failed("control socket returned no line")))
            } else {
                self.read()
            }
        }
    }

    private func decode(_ data: Data) {
        do {
            if let failure = try? JSONDecoder().decode(ControlError.self, from: data), !failure.ok {
                throw failure
            }
            finish(.success(try JSONDecoder().decode(Value.self, from: data)))
        } catch {
            finish(.failure(error))
        }
    }

    private func finish(_ result: Result<Value, Error>) {
        guard !finished else { return }
        finished = true
        connection.cancel()
        continuation.resume(with: result)
    }
}
