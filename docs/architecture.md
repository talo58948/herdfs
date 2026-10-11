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
| `crates/client` | lib + bin `herdfs` | Remote client, chunk cache, journal, access plans, FUSE. |
| `crates/agents` | bin `herdfs-agents` | Agents plugin runner. |
| `crates/lab` | lib + tests | Test lab: MinIO, toxiproxy, test OIDC issuer, authority, clients. Scenario, fault and adversary suites. |

## Data flow

**Read.** `open` → `Open` RPC returns the entry and its head version's chunk list. The client picks an access plan (below). On `read(off, len)`, the client maps the range to chunk indices, serves cached chunks, and asks `Grant` for presigned URLs of missing ones. It downloads them straight from the object store, verifies each chunk's BLAKE3 hash, and caches it. The authority checks read permission at grant time. URLs expire after the grant TTL (60 s).

**Write.** A file opened for writing gets a local journal copy. The copy is materialized from the head version unless the open truncates or the file is new. On `close`, the client fsyncs the journal copy and returns. A background committer then chunks the copy, uploads chunks, and sends `Commit`. `fsync` waits for that commit and returns `EIO` if it fails. Until a commit is acknowledged, this client reads its own journal copy (read-your-writes).

**Uploads go through the authority.** `PUT /v1/spaces/{space}/commits/{request_id}/chunks/{id}` carries at most 4 MiB. The authority verifies the hash, always writes the object, and records `(principal, id)` under the commit it is for. A commit may reference only chunks from its `base` version (which the caller must be able to read) or chunks that caller uploaded. So nobody can learn whether bytes exist elsewhere (S7), and nobody can claim a chunk they never had.

**Consistency.** The authority orders all commits. Unguarded commits make the last one the head; every version stays in history (C5). Guarded commits (`guarded: true`) fail with `Conflict` unless the head is still `base` (C6); in v0 they are API-only. Clients cache attributes and listings for 1 s. Every `open` asks the authority for the current head (close-to-open, C2).

**Forks** copy the namespace rows, ACLs and each file's head version row into a new space. No chunk bytes are copied. Time is proportional to the number of entries. A fork's history starts at the fork.

**Open files follow the latest version** (NFS-style close-to-open). Every `open` and `create` uses direct I/O, so the kernel caches no file content; the mount allows direct-I/O mappings on kernels that support them. An `open` asks for the current head; when a later `getattr` or `open` on this client sees a newer head, the client switches the inode to it, and every handle reads that version from its next read. Attributes remain cached for T_vis and are invalidated when the inode switches versions. Per-handle snapshots were tried first and fought the kernel's per-inode page cache and `mmap`.

**Paths follow renames.** Every request for a held entry names its entry id, and every request through a held directory names its parent id and child name; the authority resolves these atomically. These calls follow any rename like local `*at()` calls, while `Locate` refreshes paths used only for client bookkeeping.

## Retention and collection

The authority keeps each entry's last N versions, head included, plus any younger than T days (`--retain-versions`, `--retain-days`); the head is never pruned. A collector runs every `--gc-every` (10 min):

1. It prunes versions that fall outside retention.
2. It deletes chunk lists nothing retained uses, and a commit's upload records once its newest upload is older than the grace, so a file that takes longer than the grace to upload keeps its first chunks.
3. It sweeps the bucket: a chunk object that no retained version or recent upload has referenced for `--gc-grace` (1 h), counted from the last collection that found it referenced, is deleted. The authority persists that collection time across restarts after a successful bucket listing. While the collector runs each interval this meets the grace-plus-interval bound; after downtime or a stall, deletion completes within the grant TTL plus one interval of collection resuming.

Retention, request-id memory and sweep grace use the authority's clock. A forward step is believed: history and request IDs age by the step and deletions are not undone; only timestamps it leaves in the future are corrected. Run the authority with time synchronization that slews rather than steps.

The grace period must be at least twice the grant TTL and at least the collection interval plus the grant TTL, and exceed the time between one upload and the next or the commit, so in-flight uploads and outstanding URLs never race the sweep. `Purge` and `DeleteSpace` make history and whole spaces eligible at once. Chunks are shared across files, spaces and forks, so the live set is global.

Access to history follows S8: the authority keeps grant/revoke intervals for each ACL row. At read
time it finds the uninterrupted chain of overlapping intervals on the entry's current path for the
caller's user and token groups; versions after that chain began are visible, plus the head and the
caller's own versions. Space admins see all history.

## Limits

- Largest file: about 350 GiB (a commit's chunk list must fit the 32 MiB RPC body).
- Largest single `Grant`: 4,096 chunks.
- A client that lost a commit's answer and stays offline longer than the longer of 30 days and `--retain-days`, plus the grace, may commit that write again when it returns, over newer saves.

## Access plans

Each access independently chooses prefetch (`none`, format index, first two chunks, or whole),
sequential read-ahead (`none`, two chunks, or eight chunks), and whether accessed chunks stay
cached until that access ends. Index discovery reads bounded headers/tails and finds Parquet
footers, ZIP central directories, or an MP4 `moov`. An MP4 `moov` counts as the index when its child
boxes tile it exactly and include `mvhd`; any such `moov` up to the 8 MiB region cap is fetched whole,
whatever else it contains. Unexpected input falls back to the first and last two chunks. All bytes
use the same grant, hash verification and cache path (C8, C9).
A kept chunk damaged on disk after verification fails its hash when read and is refetched once the
authority is reachable; offline, that file is unreadable until then (P4, C11).

Fixed, size-rule and seeded-chaos deciders return a plan immediately. Jev asks the three settings
as named `choice` questions in one request. A remembered setting applies immediately; otherwise
that setting uses the rule while Jev runs. Failures, malformed answers, timeouts and low confidence
fall back per setting. A late confident answer may only add work to a still-open access. `stream`,
`fetch`, and `pin` remain names for `none/none/no`, `whole/none/no`, and `whole/none/yes`.

## Identity and permissions

- The authority validates OIDC JWTs: signature via the issuer's JWKS (discovery document), `iss`, `aud`, `exp`. In local mode it persists its own Ed25519 key beside the database, serves discovery and JWKS, and rejects a revoked `jti` immediately. The principal is `user:<sub>`; the `groups` claim adds `group:<name>` principals.
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
    # or: --local-issuer

herdfs-authority issue-token --db PATH --user NAME [--days N]
herdfs-authority revoke-token --db PATH ID

herdfs mount --authority http://127.0.0.2:7000 --space NAME --token-file PATH
    --cache DIR --cache-bytes N --journal DIR
    [--strategy stream|fetch|pin|rule|chaos:SEED|jev] [--resolve s3.lab=127.0.0.2]
    [--jev-key-file PATH] [--jev-url https://ai-gateway.vercel.sh/v1/evaluate]  # key file required with jev
    MOUNTPOINT

herdfs-agents --mount DIR --host NAME --command 'codex exec --full-auto' [--max-sessions 2] [--poll-ms 1000]
```

`--resolve` works like curl's: requests for that host name connect to the given IP, keeping the URL's port and the signed `Host` header. The lab uses it to route each client through its own toxiproxy listener (127.0.0.N) without breaking presigned URL signatures.

Agents sessions live in `/.agents/<host>/<session>/`. The session holds `prompt` (and an optional `cwd` relative to the mount); write the prompt under another name and rename it atomically to `prompt` (C7), or write it from another host, where it becomes visible on close (C2). The runner writes `status` (`running`, `done`, `failed: <reason>`) and `output`. Other hosts see `output` when the session ends, because a file becomes visible to them on close.
Permission to spawn a session is write access at its session directory, and a grant on the host directory covers every session below it.
After the runner starts, cancelling a queued session takes effect once its startup scan of the host directory finishes; the 5 s bound of A2 applies from then.
The runner tracks sessions that are direct children of the host directory. A session moved deeper, even temporarily, is no longer tracked, like a process that leaves the group with `setsid` (A2); only a principal who may spawn sessions can move it.
A2's never-back rule covers the runner's own transitions; a principal able to rewrite `status` already has A1 permission to spawn sessions; likewise, such a principal can replace or obstruct the result files the runner writes.

Client observability: `getfattr -n user.herdfs.stats MOUNT` returns JSON counters: `bytes_fetched`, `chunks_fetched`, `cache_hits`, `grants`, `commits`, `pending_commits`, `decisions` per plan setting, `jev_answers`, `jev_unsure`, `jev_errors`, and `round_trip_ms` to the authority. `herdfs explain PATH` reads `user.herdfs.plan` from the file and prints the latest access's settings, source, Jev confidence when present, and fetched bytes.

## macOS: loopback NFS

On Apple silicon macOS 26 and newer, `herdfs mount` serves NFSv3 on
`127.0.0.1` and mounts that export with the system NFS client, so it needs no
kernel extension, signed system extension or administrator password (M1).
Durable opaque handles carry the space entry id under a per-export identity;
the adapter maps them back through `Locate` rather than relying on an inode
remembered by this process.

Because NFS exposes neither open nor close, reads pin an immutable version for
one data-access epoch and advance after one second idle. An active epoch checks
the entry id at most once per second; once deletion is visible it ends and the
filehandle is stale. WRITE, size SETATTR and COMMIT are ordered per handle. They
share a whole-file write generation; FILE_SYNC and COMMIT wait for the authority
while online, while an offline COMMIT returns after the generation is sealed in
the journal (M2). Finder's AppleDouble resource forks and extended attributes
are ordinary `._` files.

The parent `$HOME/HerdFS` is mode 0700, and the random export identity is
mode 0600 without changing the permissions of its parent directory. Non-NULL
MOUNT requests require a reserved source port, while NFS requests require an
AUTH_SYS uid matching the mounting user or root. AUTH_SYS is caller-supplied and
forgeable; the actual boundary is the unguessable 128-bit export identity in
every filehandle, because MOUNT is the only operation that reveals it. This
boundary holds on Linux with its default privileged-port settings; lowering
`net.ipv4.ip_unprivileged_port_start` invalidates it.

macOS lets an unprivileged process bind a reserved port. Consequently, another
local user account can MOUNT the loopback export, obtain its identity, forge the
signed-in user's AUTH_SYS uid, and read or write that user's HerdFS drive.
The NFS client is therefore for single-user Macs (CONTRACT M1); the planned FSKit client removes the limit (docs/roadmap.md). Root and any such local user
still receive only the mounted client's principal and object-store grants
(S1-S4), not object-store credentials or access granted to another client.

The unsandboxed menu-bar app polls each mount's control socket and atomically writes Finder badge
state into the Finder extension's own sandbox container. It refuses symlinked or escaping snapshot
directories. Snapshots contain pending paths and files under kept roots, with both the traversal and
output capped at 10,000 paths per mount. A per-launch token in the snapshot authenticates immediate
Keep Downloaded and Unpin actions; the extension reads only that file and sends Finder actions to the
app as `herdfs://` URLs.

## Deliberately out of v0.1

Hard links; access times (atime is reported as mtime) and a separate ctime (reported as mtime); using a file after it is unlinked while open (fails with `ENOENT`); cross-host `flock`/`fcntl` locks (locks coordinate processes on one client only, not across clients; so a database such as SQLite must not be written from two clients at once); shared-writable `mmap` coherence across hosts; device and special files; Windows, iOS and Intel Macs; a Finder Trash on a space (Finder deletes from network volumes at once; history of a deleted file goes with it); per-uid mapping inside one mount; an S3-compatible API on herdfs; remote execution as a read-path strategy;  FS-level guarded writes. Each of these fails cleanly (`ENOTSUP`/`EPERM`), is simply absent, or, for locks, is scoped to one client as stated.
