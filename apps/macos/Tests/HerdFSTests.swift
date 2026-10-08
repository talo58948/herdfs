import Darwin
import Network
import XCTest
@testable import HerdFS

final class HerdFSTests: XCTestCase {
    func testControlResponsesDecodeDocumentedJSON() throws {
        // contract: U1, U2
        let status = try decode(MountStatus.self, """
        {"ok":true,"space":"s","mountpoint":"/p","connected":true,"round_trip_ms":12,
         "paused":false,"pending":[{"path":"a/b.txt","bytes":1234}],
         "cache":{"used":5,"budget":10},
         "kept":{"bytes":3,"pins":[{"path":"a","ready":true}]}}
        """)
        XCTAssertEqual(status.pending, [PendingUpload(path: "a/b.txt", bytes: 1234)])
        XCTAssertEqual(status.kept.bytes, 3)

        let file = try decode(FileState.self,
            #"{"ok":true,"kept":true,"downloading":false,"pending":false,"error":null}"#)
        XCTAssertTrue(file.kept)
        let failure = try decode(ControlError.self, #"{"ok":false,"error":"offline"}"#)
        XCTAssertEqual(failure.error, "offline")
        XCTAssertEqual(try decode(ControlOK.self, #"{"ok":true}"#), ControlOK())
    }

    func testAuthorityResponsesDecode() throws {
        // contract: U1, U2
        let spaces = try decode([SpaceAccess].self, #"[{"space":"team","root_perm":"admin"}]"#)
        XCTAssertEqual(spaces, [SpaceAccess(space: "team")])
        let versions = try decode([Version].self, """
        [{"id":9,"entry":4,"size":3,"author":"user:tal","created_ms":12,
          "chunks":[{"id":"ignored","len":3}]}]
        """)
        XCTAssertEqual(versions.first?.id, 9)
        let grants = try decode([AccessGrant].self,
            #"[{"path":"docs","principal":"user:tal","perm":"write"}]"#)
        XCTAssertEqual(grants.first?.perm, "write")
    }

    func testPathMapsToMostSpecificSpace() {
        // contract: U2
        let records = [
            mount(id: "one", space: "one", path: "/Users/example/HerdFS"),
            mount(id: "two", space: "two", path: "/Users/example/HerdFS/team"),
        ]
        let registry = MountRegistry(directory: URL(fileURLWithPath: "/unused"))
        let found = registry.locate(
            URL(fileURLWithPath: "/Users/example/HerdFS/team/docs/a.txt"),
            in: records
        )
        XCTAssertEqual(found?.mount.space, "two")
        XCTAssertEqual(found?.relativePath, "docs/a.txt")
        XCTAssertNil(registry.locate(URL(fileURLWithPath: "/Users/example/HerdFSs/nope"), in: records))
    }

    func testCLIArgumentsSeparateOptionsFromPositionals() throws {
        // contract: U3
        let command = try CLIArguments(
            "history",
            options: ["--authority", "https://authority"],
            positionals: ["team", "--output"],
            spaceID: "team"
        )
        XCTAssertEqual(command.rendered(extraOptions: ["--json"]), [
            "history", "--authority", "https://authority", "--json", "--", "team", "--output",
        ])
        for invalid in ["", ".", "..", "team/docs"] {
            XCTAssertThrowsError(try CLIArguments("history", positionals: [invalid], spaceID: invalid))
        }
    }

    func testAppLinkRejectsParentTraversalAndAnotherMount() throws {
        // contract: U2, U3
        let registry = MountRegistry(directory: URL(fileURLWithPath: "/unused"))
        let records = [
            mount(id: "one", space: "one", path: "/Volumes/one"),
            mount(id: "nested", space: "two", path: "/Volumes/one/nested"),
        ]
        let traversal = try XCTUnwrap(URL(string: "herdfs://one/%2E%2E/secret?action=history"))
        XCTAssertNil(AppLink.resolve(traversal, authority: "https://a", registry: registry, records: records))
        let nested = try XCTUnwrap(URL(string: "herdfs://one/nested/file?action=share"))
        XCTAssertNil(AppLink.resolve(nested, authority: "https://a", registry: registry, records: records))
        let valid = try XCTUnwrap(URL(string: "herdfs://one/docs/file?action=history"))
        XCTAssertEqual(
            AppLink.resolve(valid, authority: "https://a", registry: registry, records: records)?.path.relativePath,
            "docs/file"
        )
    }

    func testWaitingIsAControlFailure() {
        // contract: U1, U2
        XCTAssertNotNil(ControlClient.failure(for: .waiting(NWError.posix(.ETIMEDOUT))))
    }

    func testControlRequestTimesOut() async throws {
        // contract: U1, U2
        let directory = try XCTUnwrap(try? FileManager.default.url(
            for: .itemReplacementDirectory,
            in: .userDomainMask,
            appropriateFor: FileManager.default.temporaryDirectory,
            create: true
        ))
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appending(path: "control.sock").path
        let listener = try unixListener(path: path)
        defer { close(listener) }
        do {
            _ = try await ControlClient.request(
                ["op": "status"],
                from: path,
                as: ControlOK.self,
                timeout: 0.05
            )
            XCTFail("request did not time out")
        } catch ControlClientError.timedOut {
        } catch {
            XCTFail("unexpected error: \(error)")
        }
    }

    func testBadgePriority() {
        // contract: U2
        XCTAssertEqual(Badge.choose(file(kept: true)), .kept)
        XCTAssertEqual(Badge.choose(file(kept: true, downloading: true)), .downloading)
        XCTAssertEqual(Badge.choose(file(kept: true, downloading: true, pending: true)), .pending)
        XCTAssertEqual(Badge.choose(file(kept: true, pending: true, error: "failed")), .error)
        XCTAssertNil(Badge.choose(file()))
    }

    private func decode<Value: Decodable>(_ type: Value.Type, _ json: String) throws -> Value {
        try JSONDecoder().decode(type, from: Data(json.utf8))
    }

    private func mount(id: String, space: String, path: String) -> MountRecord {
        MountRecord(id: id, space: space, authority: "https://a", mountpoint: path,
                    socket: "/tmp/\(id).sock", pid: 1)
    }

    private func file(
        kept: Bool = false,
        downloading: Bool = false,
        pending: Bool = false,
        error: String? = nil
    ) -> FileState {
        FileState(kept: kept, downloading: downloading, pending: pending, error: error)
    }

    private func unixListener(path: String) throws -> Int32 {
        let descriptor = socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw POSIXError(.ENOTSOCK) }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8) + [0]
        guard bytes.count <= MemoryLayout.size(ofValue: address.sun_path) else {
            close(descriptor)
            throw POSIXError(.ENAMETOOLONG)
        }
        withUnsafeMutableBytes(of: &address.sun_path) { buffer in
            buffer.copyBytes(from: bytes)
        }
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(descriptor, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard result == 0, listen(descriptor, 1) == 0 else {
            close(descriptor)
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EINVAL)
        }
        return descriptor
    }
}
