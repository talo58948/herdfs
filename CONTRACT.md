# Contract (v0)

Every test names the lines it checks with a `// contract: C9, S2` comment. Code that serves no line here does not belong in v0.

"Commit" means the authority has acknowledged a new version. Parameters: T_vis = 1 s, grant TTL = 5 min, decision budget = 300 ms.

## Data and consistency

| ID | Promise |
|---|---|
| C1 | Read-your-writes on the same client after `close` or `fsync`. |
| C2 | Close-to-open: an `open` on B that starts T_vis after A's commit sees that version or a newer one. |
| C3 | Each commit is one immutable version. Readers never see a partial version. |
| C4a | After `close` returns, data survives a client crash (journal) and commits after restart. |
| C4b | After commit, data survives a crash of any component. |
| C5 | Concurrent unguarded commits: the last becomes head, and all remain in history. |
| C6 | Concurrent guarded commits on the same base: exactly one succeeds; the rest get `Conflict`. API-level in v0. |
| C7 | Rename is atomic within a space; a directory rename moves the whole subtree at once. |
| C8 | Every access strategy returns identical bytes for the same version. |
| C9 | A cold read of `[off, off+len)` fetches at most the chunks overlapping that range, plus configured prefetch. |
| C10 | Mounting a space uses local disk proportional to metadata and cache, not to data size. |
| C11 | The cache never serves bytes of a different version. A tampered cached chunk fails its hash and is refetched. |
| C12 | Content writes made offline to existing files are journaled and committed on reconnect, under C5. Namespace changes (create, mkdir, rename, remove) need the authority and fail with `EIO` while it is unreachable. |
| C13 | A fork copies no chunk bytes, includes all commits acknowledged before it, and is isolated from its parent afterwards. Its time grows with entry count only (≤ 1 s per 100k entries). |
| C14 | Any version in an entry's history is readable. Restore creates a new head and keeps history. |

## Security

| ID | Promise |
|---|---|
| S1 | Clients hold no object-store credentials. The authority is the only enforcement point. |
| S2 | A grant needs read permission at grant time, expires within the TTL, and covers only chunks of a version of that entry. |
| S3 | Root on a client gets no bytes beyond what that client's principal was granted. |
| S4 | Revocation stops new grants immediately; outstanding grants die within the TTL. |
| S5 | Tokens are checked for signature, `iss`, `aud` and `exp`; `alg=none` and unknown keys are rejected. |
| S6 | Whether bytes exist anywhere in the store is not observable: uploads always happen, and commits may reference only chunks from a readable base or uploaded by the committer. |

## Strategy and plugins

| ID | Promise |
|---|---|
| P1 | The strategy is decided at `open` only, within the 300 ms budget; otherwise the default applies. |
| P2 | If Jev errors, times out or is unsure, the default applies and the filesystem stays available. |
| A1 | The agents plugin uses only the public filesystem. Permission to spawn on a host is write access on `/.agents/<host>/`. |
