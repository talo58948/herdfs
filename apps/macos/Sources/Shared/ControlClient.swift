import Foundation
import Network

enum ControlClientError: LocalizedError {
    case closed
    case timedOut

    var errorDescription: String? {
        switch self {
        case .closed: "Control socket closed"
        case .timedOut: "Control socket timed out"
        }
    }
}

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

    static func request<Value: Decodable & Sendable>(
        _ payload: [String: String],
        from socket: String,
        as type: Value.Type,
        timeout: TimeInterval = 2
    ) async throws -> Value {
        var data = try JSONSerialization.data(withJSONObject: payload)
        data.append(0x0A)
        return try await withCheckedThrowingContinuation { continuation in
            LineRequest(
                data: data,
                socket: socket,
                timeout: timeout,
                continuation: continuation
            ).start()
        }
    }

    static func failure(for state: NWConnection.State) -> Error? {
        switch state {
        case .waiting(let error), .failed(let error): error
        case .cancelled: ControlClientError.closed
        default: nil
        }
    }
}

private final class LineRequest<Value: Decodable & Sendable>: @unchecked Sendable {
    private let connection: NWConnection
    private let data: Data
    private let timeout: TimeInterval
    private let continuation: CheckedContinuation<Value, Error>
    private let queue = DispatchQueue(label: "dev.herdfs.HerdFS.control")
    private var received = Data()
    private var timer: DispatchSourceTimer?
    private var finished = false

    init(
        data: Data,
        socket: String,
        timeout: TimeInterval,
        continuation: CheckedContinuation<Value, Error>
    ) {
        self.data = data
        self.timeout = timeout
        self.continuation = continuation
        connection = NWConnection(to: .unix(path: socket), using: .tcp)
    }

    func start() {
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + timeout)
        timer.setEventHandler { self.finish(.failure(ControlClientError.timedOut)) }
        timer.resume()
        self.timer = timer
        connection.stateUpdateHandler = { state in
            if case .ready = state {
                self.send()
            } else if let error = ControlClient.failure(for: state) {
                self.finish(.failure(error))
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
                self.finish(.failure(ControlClientError.closed))
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
        timer?.cancel()
        timer = nil
        connection.stateUpdateHandler = nil
        connection.cancel()
        continuation.resume(with: result)
    }
}
