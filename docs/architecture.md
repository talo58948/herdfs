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

**Read.** `open` → `Open` RPC returns the entry and its head version's chunk list. The client picks an access strategy (below). On `read(off, len)`, the client maps the range to chunk indices, serves cached chunks, and asks `Grant` for presigned URLs of missing ones. It downloads them straight from the object store, verifies each chunk's BLAKE3 hash, and caches it. The authority checks read permission at grant time. URLs expire after the grant TTL (60 s).

**Write.** A file opened for writing gets a local journal copy. The copy is materialized from the head version unless the open truncates or the file is new. On `close`, the client fsyncs the journal copy and returns. A background committer then chunks the copy, uploads chunks, and sends `Commit`. `fsync` waits for that commit and returns `EIO` if it fails. Until a commit is acknowledged, this client reads its own journal copy (read-your-writes).

**Uploads go through the authority.** `PUT /v1/spaces/{space}/commits/{request_id}/chunks/{id}` carries at most 4 MiB. The authority verifies the hash, always writes the object, and records `(principal, id)` under the commit it is for. A commit may reference only chunks from its `base` version (which the caller must be able to read) or chunks that caller uploaded. So nobody can learn whether bytes exist elsewhere (S7), and nobody can claim a chunk they never had.

**Consistency.** The authority orders all commits. Unguarded commits make the last one the head; every version stays in history (C5). Guarded commits (`guarded: true`) fail with `Conflict` unless the head is still `base` (C6); in v0 they are API-only. Clients cache attributes and listings for 1 s. Every `open` asks the authority for the current head (close-to-open, C2).

**Forks** copy the namespace rows, ACLs and each file's head version row into a new space. No chunk bytes are copied. Time is proportional to the number of entries. A fork's history starts at the fork.

**Open files follow the latest version** (NFS-style close-to-open). Every `open` and `create` uses direct I/O, so the kernel caches no file content; the mount allows direct-I/O mappings on kernels that support them. An `open` asks for the current head; when a later `getattr` or `open` on this client sees a newer head, the client switches the inode to it, and every handle reads that version from its next read. Attributes remain cached for T_vis and are invalidated when the inode switches versions. Per-handle snapshots were tried first and fought the kernel's per-inode page cache and `mmap`.

**Paths follow renames.** Operations name entries by path, but the client knows every inode's entry id. A cached inode path older than T_vis is refreshed with `Locate` (by entry id) before use.

## Retention and collection

The authority keeps each entry's last N versions, head included, plus any younger than T days (`--retain-versions`, `--retain-days`); the head is never pruned. A collector runs every `--gc-every` (10 min):

1. It prunes versions that fall outside retention.
2. It deletes chunk lists nothing retained uses, and a commit's upload records once its newest upload is older than the grace, so a file that takes longer than the grace to upload keeps its first chunks.
3. It sweeps the bucket: a chunk object that no retained version or recent upload has referenced for `--gc-grace` (1 h), counted from the last collection that found it referenced, is deleted. The authority persists that collection time across restarts after a successful bucket listing. While the collector runs each interval this meets the grace-plus-interval bound; after downtime or a stall, deletion completes within the grant TTL plus one interval of collection resuming.

The grace period must be at least twice the grant TTL and at least the collection interval plus the grant TTL, and exceed the time between one upload and the next or the commit, so in-flight uploads and outstanding URLs never race the sweep. `Purge` and `DeleteSpace` make history and whole spaces eligible at once. Chunks are shared across files, spaces and forks, so the live set is global.

Access to history follows S8: each ACL row records the highest committed version ID when it was granted, and a principal sees only later versions plus the head unless it is an admin.

## Limits

- Largest file: about 350 GiB (a commit's chunk list must fit the 32 MiB RPC body).
- Largest single `Grant`: 4,096 chunks.
- A client that lost a commit's answer and stays offline longer than the longer of 30 days and `--retain-days`, plus the grace, may commit that write again when it returns, over newer saves.

## Access strategies

Chosen once per open by a `Decider`, never per read:

| Strategy | At open | Eviction |
|---|---|---|
| `Stream` | nothing | LRU |
| `Fetch` | prefetch every chunk in the background | LRU |
| `Pin` | prefetch every chunk | never evicted while pinned |

All strategies read through the same verified chunk cache, so they return identical bytes (C8). Deciders: fixed, size rule, chaos (seeded random, for tests), and Jev through Vercel AI Gateway. `open` never waits for Jev: it takes Jev's remembered answer for a similar open (the same process and extension, and the same phrases for size, cached share, recent opens, round trip and download time), or the size rule's choice while Jev is asked in the background. Errors, a 2 s timeout and low confidence leave the rule's choice (P2). A confident later answer upgrades a still-open Stream handle to Fetch or Pin.

## Identity and permissions

- The authority validates OIDC JWTs: signature via the issuer's JWKS (discovery document), `iss`, `aud`, `exp`. The principal is `user:<sub>`; the `groups` claim adds `group:<name>` principals.
- ACL rows are `(space, path, principal, perm)`. A principal's permission on a path is the highest perm granted on that path or any ancestor. Perms: `read` < `write` < `admin`. The creator of a space gets `admin` on its root.
- Read ops need `read` on the path. Mutations need `write` on the path; for create, mkdir and rename, on both source and destination. `SetAcl` and `Fork` need `admin`.
- ACL rows attach to paths, not entries. They stay where they are when an entry is renamed or removed, so a moved subtree takes the grants of its new place, and a new entry at an old path inherits that path's grants.
- Entry ids are not secret: a commit to an id that does not exist says so.
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
    [--jev-key-file PATH] [--jev-url https://ai-gateway.vercel.sh/v1/evaluate]  # key file required with jev
    MOUNTPOINT

herdfs-agents --mount DIR --host NAME --command 'codex exec --full-auto' [--max-sessions 2] [--poll-ms 1000]
```

`--resolve` works like curl's: requests for that host name connect to the given IP, keeping the URL's port and the signed `Host` header. The lab uses it to route each client through its own toxiproxy listener (127.0.0.N) without breaking presigned URL signatures.

Agents sessions live in `/.agents/<host>/<session>/`. The session holds `prompt` (and an optional `cwd` relative to the mount); the runner writes `status` (`running`, `done`, `failed: <reason>`) and `output`. Other hosts see `output` when the session ends, because a file becomes visible to them on close.

Client observability: `getfattr -n user.herdfs.stats MOUNT` returns JSON counters: `bytes_fetched`, `chunks_fetched`, `cache_hits`, `grants`, `commits`, `pending_commits`, `decisions` per strategy, `jev_answers`, `jev_unsure`, `jev_errors`, and `round_trip_ms` to the authority.

## Deliberately out of v0

Hard links; access times (atime is reported as mtime) and a separate ctime (reported as mtime); using a file after it is unlinked while open (fails with `ENOENT`); cross-host `flock`/`fcntl` locks (locks coordinate processes on one client only, not across clients; so a database such as SQLite must not be written from two clients at once); shared-writable `mmap` coherence across hosts; device and special files; macOS and Windows; per-uid mapping inside one mount; an S3-compatible API on herdfs; remote execution as a read-path strategy;  FS-level guarded writes. Each of these fails cleanly (`ENOTSUP`/`EPERM`), is simply absent, or, for locks, is scoped to one client as stated.
