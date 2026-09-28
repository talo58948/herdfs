# Contract (v0)

Every test names the lines it checks with a `// contract: C9, S2` comment. Code that serves no line here does not belong in v0.

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
| C9 | A cold read of `[off, off+len)` fetches at most the chunks overlapping that range, plus configured prefetch. |
| C10 | Mounting a space uses local disk proportional to metadata and cache, not to data size. |
| C11 | The cache never serves bytes of a different version. A tampered cached chunk fails its hash and is refetched. |
| C12 | Content writes made offline to existing files are journaled and committed on reconnect, under C5, when the write replaces the file (truncate) or this client holds the file's current bytes. Namespace changes (create, mkdir, rename, remove) need the authority and fail with `EIO` while it is unreachable. |
| C13 | A fork copies no chunk bytes, includes all commits acknowledged before it, and is isolated from its parent afterwards. Its time grows with entry count only (≤ 1 s per 100k entries). |
| C14 | Every retained version in an entry's history is readable (subject to S8). Retention keeps the head always, plus the last N versions or those younger than T days (authority settings). Restore creates a new head and keeps history. |
| C15 | The store holds only chunks of retained versions, plus uploads younger than the grace period: storage is bounded by what retention keeps, not by how much was ever written. |

## Security

| ID | Promise |
|---|---|
| S1 | Clients hold no object-store credentials. The authority is the only enforcement point. |
| S2 | A grant needs read permission at grant time, expires within the TTL, and covers only chunks of a version of that entry. |
| S3 | Root on a client gets no bytes beyond what that client's principal was granted. |
| S4 | Revocation stops new grants immediately; outstanding grants die within the grant TTL (60 s). |
| S5 | Tokens are checked for signature, `iss`, `aud` and `exp` (no leeway beyond 5 s); `alg=none` and unknown keys are rejected; a key the issuer withdraws stops being accepted within 5 minutes. |
| S6 | Whether bytes exist anywhere in the store is not observable through the protocol: uploads always happen and get identical answers, and commits may reference only chunks from a readable base or uploaded by the committer. Known residual: the store's write latency can differ for bytes it already holds (measured median +4 ms of 52 ms on MinIO), which the R4 timing test bounds. |

| S7 | Deleting a file, purging its history, deleting a space, or a version leaving retention erases the bytes only it referenced from the store within grace + interval. Chunks still referenced by a retained version anywhere stay. |
| S8 | A principal can read an entry's current head, and older versions only if they were created after it gained read access there (admins of the space see all). Granting access later does not reveal earlier history. |

## Strategy and plugins

| ID | Promise |
|---|---|
| P1 | The strategy is decided at `open` only, and `open` never waits for Jev: a remembered answer applies at once; otherwise the default applies and Jev's later answer may start a prefetch for that open. |
| P2 | If Jev errors, times out or is unsure, the default applies and the filesystem stays available. |
| A1 | The agents plugin uses only the public filesystem. Permission to spawn on a host is write access on `/.agents/<host>/`. |
