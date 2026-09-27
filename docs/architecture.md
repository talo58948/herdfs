# Architecture (v0)

herdfs is a shared filesystem. Three runtimes:

- **Object store**: any S3-compatible bucket. Holds chunk bytes at `chunks/<blake3-hex>`. Private; only the authority holds its credentials.
- **Authority** (`herdfs-authority`): the single source of truth for one or more spaces. It owns the namespace, versions, ACLs and grants, stores metadata in SQLite, and checks every request. It is off the read path for bytes.
- **Clients** (`herdfs mount`): FUSE mounts. Each mount is one principal on one space. A client caches chunks locally and journals writes.

Plugins are separate programs that use a mount like any other program (the agents runner, `herdfs-agents`).

## Crates

| Crate | Kind | Owns |
|---|---|---|
| `crates/core` | lib | Protocol types, `SpacePath`, chunk ids and chunking. No I/O. |
| `crates/authority` | lib + bin | `Authority`: a synchronous state machine over a `rusqlite::Connection`. `server`: axum, JWT validation, object store, URL signing. |
| `crates/client` | lib + bin `herdfs` | Remote client, chunk cache, journal, access strategies, FUSE. |
| `crates/agents` | bin `herdfs-agents` | Agents plugin runner. |
| `crates/lab` | lib + tests | Test lab: MinIO, toxiproxy, test OIDC issuer, authority, clients. Scenario, fault and adversary suites. |

## Data flow

**Read.** `open` → `Open` RPC returns the entry and its head version's chunk list. The client picks an access strategy (below). On `read(off, len)`, the client maps the range to chunk indices, serves cached chunks, and asks `Grant` for presigned URLs of missing ones. It downloads them straight from the object store, verifies each chunk's BLAKE3 hash, and caches it. The authority checks read permission at grant time. URLs expire after 5 minutes.

**Write.** A file opened for writing gets a local journal copy. The copy is materialized from the head version unless the open truncates or the file is new. On `close`, the client fsyncs the journal copy and returns. A background committer then chunks the copy, uploads chunks, and sends `Commit`. `fsync` waits for that commit and returns `EIO` if it fails. Until a commit is acknowledged, this client reads its own journal copy (read-your-writes).

**Uploads go through the authority.** `PUT /v1/spaces/{space}/chunks/{id}` carries at most 4 MiB. The authority verifies the hash, always writes the object, and records `(principal, id)`. A commit may reference only chunks from its `base` version (which the caller must be able to read) or chunks that caller uploaded. So nobody can learn whether bytes exist elsewhere (S7), and nobody can claim a chunk they never had.

**Consistency.** The authority orders all commits. Unguarded commits make the last one the head; every version stays in history (C5). Guarded commits (`guarded: true`) fail with `Conflict` unless the head is still `base` (C6); in v0 they are API-only. Clients cache attributes and listings for 1 s. Every `open` asks the authority for the current head (close-to-open, C2).

**Forks** copy the namespace rows, ACLs and each file's head version row into a new space. No chunk bytes are copied. Time is proportional to the number of entries. A fork's history starts at the fork.

## Access strategies

Chosen once per open by a `Decider`, never per read:

| Strategy | At open | Eviction |
|---|---|---|
| `Stream` | nothing | LRU |
| `Fetch` | prefetch every chunk in the background | LRU |
| `Pin` | prefetch every chunk | never evicted while pinned |

All strategies read through the same verified chunk cache, so they return identical bytes (C8). Deciders: fixed, size rule, chaos (seeded random, for tests), and Jev through Vercel AI Gateway with a 300 ms budget and a fixed default on timeout or low confidence.

## Identity and permissions

- The authority validates OIDC JWTs: signature via the issuer's JWKS (discovery document), `iss`, `aud`, `exp`. The principal is `user:<sub>`; the `groups` claim adds `group:<name>` principals.
- ACL rows are `(space, path, principal, perm)`. A principal's permission on a path is the highest perm granted on that path or any ancestor. Perms: `read` < `write` < `admin`. The creator of a space gets `admin` on its root.
- Read ops need `read` on the path. Mutations need `write` on the path; for create, mkdir and rename, on both source and destination. `SetAcl` and `Fork` need `admin`.
- One mount is one principal. POSIX mode bits are stored and reported, not enforced. Every entry is shown as owned by the mounting user.

## Binaries (the lab depends on these flags)

```
herdfs-authority --listen 127.0.0.1:7000 --db PATH
    --s3-endpoint http://127.0.0.1:9000 --s3-public-endpoint http://s3.lab:9000
    --s3-bucket herdfs --s3-region us-east-1          # keys: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
    --oidc-issuer http://127.0.0.1:7100 --oidc-audience herdfs

herdfs mount --authority http://127.0.0.2:7000 --space NAME --token-file PATH
    --cache DIR --cache-bytes N --journal DIR
    [--strategy stream|fetch|pin|rule|chaos:SEED|jev] [--resolve s3.lab=127.0.0.2]
    MOUNTPOINT

herdfs-agents --mount DIR --host NAME --command 'codex exec --full-auto' [--max-sessions 2] [--poll-ms 1000]
```

`--resolve` works like curl's: requests for that host name connect to the given IP, keeping the URL's port and the signed `Host` header. The lab uses it to route each client through its own toxiproxy listener (127.0.0.N) without breaking presigned URL signatures.

Agents sessions live in `/.agents/<host>/<session>/`. The session holds `prompt` (and an optional `cwd` relative to the mount); the runner writes `status` (`running`, `done`, `failed: <reason>`) and `output`. Other hosts see `output` when the session ends, because a file becomes visible to them on close.

Client observability: `getfattr -n user.herdfs.stats MOUNT` returns JSON counters: `bytes_fetched`, `chunks_fetched`, `cache_hits`, `grants`, `commits`, `pending_commits`, `decisions` per strategy.

## Deliberately out of v0

Hard links; cross-host `flock`/`fcntl` locks; shared-writable `mmap` coherence across hosts; device and special files; macOS and Windows; per-uid mapping inside one mount; an S3-compatible API on herdfs; remote execution as a read-path strategy; chunk garbage collection; FS-level guarded writes. Each of these must fail cleanly (`ENOTSUP`/`EPERM`) or be simply absent.
