# Contract (v0.1)

Every test names the lines it checks with a `// contract: C9, S2` comment. Code that serves no line here does not belong in herdfs.

"Commit" means the authority has acknowledged a new version. Parameters: T_vis = 1 s, grant TTL = 60 s, Jev timeout = 2 s, collection grace = 1 h, collection interval = 10 min.

## Data and consistency

| ID | Promise |
|---|---|
| C1 | Read-your-writes on the same client after `close` or `fsync`. |
| C2 | Close-to-open: an `open` on B that starts T_vis after A's commit sees that version or a newer one. A close starts the commit; `fsync` waits for it. A file's mtime is the time of its last write, as on a local disk, so a tool on B that compares times (make) must run after A's `fsync`, not merely after its close. |
| C3 | Each commit is one immutable version. A read never returns bytes of a partially committed version. An open file follows its latest version between reads, as a local file does when another program rewrites it; one large `read` may span that switch, so every byte comes from some committed version. |
| C4a | After `close` returns, data survives a client crash (journal) and commits after restart. |
| C4b | After commit, data survives a crash of any component. |
| C5 | Concurrent unguarded commits: the last becomes head, and all remain in history. |
| C6 | Concurrent guarded commits on the same base: exactly one succeeds; the rest get `Conflict`. API-level in v0. |
| C7 | Rename is atomic within a space; a directory rename moves the whole subtree at once. Other clients follow a rename within T_vis: operations on an entry they hold reach it at its new path. |
| C8 | Every access strategy returns identical bytes for the same version. |
| C9 | A cold read of `[off, off+len)` fetches at most the chunks overlapping that range, plus the prefetch and read-ahead of its access plan (P3). |
| C10 | Mounting a space uses local disk proportional to metadata, cache and what the user keeps downloaded (P4), not to data size. |
| C11 | The cache never serves bytes of a different version. A tampered cached chunk fails its hash and is refetched. |
| C12 | Content writes made offline to existing files are journaled and committed on reconnect, under C5, when the write replaces the file (truncate) or this client holds the file's current bytes. Namespace changes (create, mkdir, rename, remove) need the authority and fail with `EIO` while it is unreachable. |
| C13 | A fork copies no chunk bytes, includes all commits acknowledged before it, and is isolated from its parent afterwards. Its time grows with entry count only (≤ 1 s per 100k entries). |
| C14 | Every retained version in an entry's history is readable (subject to S8). Retention keeps the last N versions (head included) plus any younger than T days (authority settings); the head is never pruned. Restore creates a new head and keeps history. |
| C15 | The store holds only chunks of retained versions, plus uploads for commits whose newest upload is younger than the grace period: storage is bounded by what retention keeps, not by how much was ever written. |

## Security

| ID | Promise |
|---|---|
| S1 | Clients hold no object-store credentials. The authority is the only enforcement point. |
| S2 | A grant needs read permission at grant time, expires within the TTL, and covers only chunks of a version of that entry. |
| S3 | Root on a client gets no bytes beyond what that client's principal was granted. |
| S4 | Revocation stops new grants immediately; outstanding grants die within the grant TTL (60 s). |
| S5 | OIDC and built-in local-issuer tokens are checked for signature, `iss`, `aud` and `exp` (no leeway beyond 5 s); `alg=none` and unknown keys are rejected; a key an external issuer withdraws stops being accepted within 5 minutes. Local tokens carry an issued `jti`. Revoking it stops acceptance immediately for new requests; grants already issued expire per S4. |
| S6 | Whether bytes exist anywhere in the store is not observable through the protocol: uploads always happen and get identical answers, and commits may reference only chunks from a readable base or uploaded by the committer. Known residual: the store's write latency can differ for bytes it already holds (measured median +4 ms of 52 ms on MinIO), which the R4 timing test bounds. |

| S7 | While collection runs every interval, deleting a file, purging its history, deleting a space, or a version leaving retention erases the bytes only it referenced from the store within grace + interval; after downtime or a stalled collector, erasure completes within the grant TTL plus one interval of collections resuming. The objects are deleted and a read with the store's own credentials returns 404. Chunks still referenced by a retained version anywhere stay. Physical media erasure after that is the store's: MinIO empties its trash about 5 minutes later; S3 follows its own deletion policy. |
| S8 | A principal can read an entry's current head, and older versions only if they were created after it gained read access at the entry's current path (admins of the space see all). Granting access later does not reveal earlier history, and no request (grant, restore, commit base) lets a principal use or confirm a version it cannot see. History follows the path: moving an entry is its mover's disclosure decision. A group grant counts from when the group was granted, since membership belongs to the identity provider. "After" is the authority's commit order, not wall-clock time. The author of a commit may always learn that it landed and see its own bytes. |

## Strategy and plugins

| ID | Promise |
|---|---|
| P1 | An access plan is decided when a file is opened (on macOS, at its first access after 1 s without one), and that never waits for Jev: a remembered answer applies at once; otherwise the rule's plan applies, and Jev's later answer may add prefetch or read-ahead for that access, never take any away. |
| P2 | If Jev errors, times out or is unsure about a setting, the rule's value for that setting applies and the filesystem stays available. Byte ranges are computed by the client from the file, never taken from Jev. |
| P3 | An access plan sets three things independently: what to prefetch when the access starts (nothing, the regions holding the file's index, its first window, or all of it), read-ahead after sequential reads (none, small, large), and whether its chunks stay cached until the access ends. The index regions are found from the format (Parquet footer, ZIP central directory, MP4 `moov`), else the first and last window. `herdfs explain PATH` shows the plan of the file's latest access, where it came from (remembered, rule or Jev) and the bytes it fetched. |
| P4 | A file or folder the user keeps downloaded has the latest version of everything below it cached, and readable offline once `herdfs status` reports it ready, for as long as that mount runs (mounting itself needs the authority); it is never evicted until the user unpins it. Only kept-downloaded bytes may exceed the cache budget. |
| A1 | The agents plugin uses only the public filesystem. Permission to spawn a session is write access at its directory under `/.agents/<host>/`; a grant on `/.agents/<host>/` covers every session there. |
| A2 | A session's `status` moves from `queued` to `running` to `done`, `failed: <reason>` or `cancelled`, never back. Writing a `cancel` file stops it within 5 s: the session's process group is killed (a process that leaves the group with `setsid` is not tracked). When its command exits, the rest of its process group is killed. The runner writes `exit_code`, `stdout` and `stderr` beside `output`. |

## macOS and the desktop

| ID | Promise |
|---|---|
| M1 | On macOS (Apple silicon, 26 or newer) a space mounts without a kernel extension, system extension, Apple-signed code or admin password: the client serves it to the system's NFS client on loopback. Every line above holds on macOS except as M2 states. |
| M2 | On macOS the client sees no open or close. A file's access starts at its first read after 1 s without a read or write, and stays on one version until the file has been idle for 1 s: a reopen during it may keep the old version, and an open file used after a pause may move to a newer one (this replaces C3's "between reads"). Close and `fsync` both wait for the commit while the authority is reachable; while it is not, they return once the journal holds the write (C12). Access plans see no process name. Finder's extended attributes and resource forks are kept in `._` files, stored like any other file. |
| U1 | The menu-bar app signs in through the browser and keeps the refresh token in the Keychain, mounts the user's spaces at login, and shows for each space whether it is mounted and connected, its pending uploads (which can be paused: while paused, `fsync` and close return once the journal holds the write, as when offline under C12, and the commits resume with uploads), and its cache and kept-downloaded use. |
| U2 | In Finder, files in a space carry a badge when kept downloaded, downloading, waiting to upload, or failing, and offer Keep Downloaded, Unpin, Copy Link, Version History and Share. |
| U3 | Everything a principal may do through the authority (history, restore, grants, forks) is available from the `herdfs` CLI; the app's Finder actions run the same commands. |
