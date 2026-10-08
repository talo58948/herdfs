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
        XCTAssertEqual(status.roundTripMs, 12)
        XCTAssertEqual(status.kept.pins, [KeptPath(path: "a", ready: true)])

        let file = try decode(FileState.self,
            #"{"ok":true,"kept":true,"downloading":false,"pending":false,"error":null}"#)
        XCTAssertTrue(file.kept)
        let failure = try decode(ControlError.self, #"{"ok":false,"error":"offline"}"#)
        XCTAssertEqual(failure.error, "offline")
        XCTAssertEqual(try decode(ControlOK.self, #"{"ok":true}"#), ControlOK(ok: true))
    }

    func testAuthorityResponsesDecode() throws {
        // contract: U1, U2
        let spaces = try decode([SpaceAccess].self, #"[{"space":"team","root_perm":"admin"}]"#)
        XCTAssertEqual(spaces, [SpaceAccess(space: "team", rootPerm: "admin")])
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
        let found = MountRegistry(directory: URL(fileURLWithPath: "/unused"))
            .locate(URL(fileURLWithPath: "/Users/example/HerdFS/team/docs/a.txt"), in: records)
        XCTAssertEqual(found?.mount.space, "two")
        XCTAssertEqual(found?.relativePath, "docs/a.txt")
        XCTAssertNil(MountRegistry(directory: URL(fileURLWithPath: "/unused"))
            .locate(URL(fileURLWithPath: "/Users/example/HerdFSs/nope"), in: records))
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
        FileState(ok: true, kept: kept, downloading: downloading, pending: pending, error: error)
    }
}
