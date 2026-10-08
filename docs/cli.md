# Command reference

## `herdfs`

`herdfs` mounts spaces, signs in and sends authenticated commands to the authority. It supports `-h`, `--help`, `-V` and `--version`.

`--json` is a global flag. It makes `login` and authority commands print machine-readable JSON. `show --json` also requires `--output`, because file bytes cannot share standard output with JSON. `mount`, `explain` and `logout` accept the flag but do not change their output; `explain` already prints JSON.

Authenticated commands use the token saved by `login` at `~/.config/herdfs/token`. Pass `--token-file PATH` to use another file. A legacy file containing one raw JWT also works. Login token files refresh shortly before expiry and retry once with a refresh after HTTP 401.

Commands that contact the authority require `--authority URL`. They also accept repeatable `--resolve HOST=IP`, which connects to the IP while retaining the URL's port and host.

### `mount`

Mounts one space on Linux and stays in the foreground until unmounted, SIGINT or SIGTERM. macOS support is in progress.

```sh
herdfs mount \
  --authority http://127.0.0.1:7000 \
  --space demo \
  --cache /var/cache/herdfs/demo \
  --cache-bytes 1073741824 \
  --journal /var/lib/herdfs/demo-journal \
  /mnt/demo
```

| Argument | Default | Meaning |
|---|---:|---|
| `--authority URL` | required | Authority base URL. A trailing slash is accepted. |
| `--space NAME` | required | Space to mount. |
| `--token-file PATH` | login file | Token file to read and refresh. |
| `--cache DIR` | required | On-disk verified chunk-cache directory. It is created if absent. |
| `--cache-bytes BYTES` | required | LRU cache limit. Chunks kept for an access may exceed it. |
| `--journal DIR` | required | Directory for writes not yet committed. |
| `--strategy VALUE` | `rule` | `stream`, `fetch`, `pin`, `rule`, `chaos:SEED`, `jev`, or explicit `PREFETCH/READ_AHEAD/KEEP`. |
| `--jev-key-file PATH` | none | Vercel AI Gateway bearer-key file. Required with `--strategy jev`. |
| `--jev-url URL` | `https://ai-gateway.vercel.sh/v1/evaluate` | Jev evaluation endpoint. |
| `--resolve HOST=IP` | none | Connection override. Repeatable. |
| `MOUNTPOINT` | required | Existing mount-point directory. |

See [Access plans and Jev](strategies.md) for plan values and behavior. `RUST_LOG` controls tracing; the default is `info`.

### `login`

Starts an OIDC authorization-code login with PKCE. It prints the authorization URL, tries to open it in a browser, waits up to five minutes on a loopback callback, and saves the access and refresh tokens with mode `0600`.

```sh
herdfs login --issuer https://issuer.example --client-id herdfs-cli
```

`--issuer URL` and `--client-id ID` are required. `--token-file PATH` overrides the login file.

### `logout`

Deletes the saved login token file. It succeeds if the file is already absent.

```sh
herdfs logout
```

`--token-file PATH` selects a different token file.

### `whoami`

Prints the authenticated `user:` principal, then each `group:` principal.

```sh
herdfs whoami --authority http://127.0.0.1:7000
```

### `spaces`

Lists reachable spaces and the principal's permission at each root.

```sh
herdfs spaces --authority http://127.0.0.1:7000
```

### `history`

Lists visible versions oldest first. Paths are relative to the space, without a leading slash. The root path is the empty string.

```sh
herdfs history --authority http://127.0.0.1:7000 demo docs/report.txt
```

### `show`

Downloads one visible version to standard output or to `--output FILE`. A file output is replaced only after the download succeeds.

```sh
herdfs show --authority http://127.0.0.1:7000 \
  demo docs/report.txt --version 42 --output report.txt
```

### `restore`

Restores one visible version as a new head. It keeps the existing history.

```sh
herdfs restore --authority http://127.0.0.1:7000 \
  demo docs/report.txt --version 42
```

### `share`

Sets an explicit `read`, `write` or `admin` grant for a `user:` or `group:` principal at a path.

```sh
herdfs share --authority http://127.0.0.1:7000 \
  demo docs user:bob write
```

### `revoke`

Removes one principal's explicit grant at a path.

```sh
herdfs revoke --authority http://127.0.0.1:7000 \
  demo docs user:bob
```

### `access`

Lists explicit grants at a path and its ancestors.

```sh
herdfs access --authority http://127.0.0.1:7000 demo docs/report.txt
```

### `fork`

Forks a space under a new name without copying chunk bytes.

```sh
herdfs fork --authority http://127.0.0.1:7000 demo demo-snapshot
```

### `create-space`

Creates a space. Its creator receives `admin` on the root.

```sh
herdfs create-space --authority http://127.0.0.1:7000 demo
```

### `delete-space`

Deletes a space and makes bytes referenced only by it eligible for collection.

```sh
herdfs delete-space --authority http://127.0.0.1:7000 demo-snapshot
```

### `purge`

Deletes every non-head version at a path. The head remains.

```sh
herdfs purge --authority http://127.0.0.1:7000 demo docs/report.txt
```

### `explain`

Prints the latest access plan, source, Jev answer when present and fetched-byte count for a mounted file.

```sh
herdfs explain /mnt/demo/docs/report.txt
```

The file must have a recorded access since this mount started.

## `herdfs-authority`

```text
herdfs-authority [OPTIONS] --listen ADDRESS --db PATH \
  --s3-endpoint URL --s3-public-endpoint URL --s3-bucket NAME \
  --s3-region REGION --oidc-issuer URL --oidc-audience AUDIENCE
```

| Flag | Default | Meaning |
|---|---:|---|
| `--listen ADDRESS` | required | Socket address on which the HTTP server listens. |
| `--db PATH` | required | SQLite metadata database, created if absent. |
| `--s3-endpoint URL` | required | S3-compatible endpoint where the authority writes chunks. |
| `--s3-public-endpoint URL` | required | Endpoint placed in presigned chunk-download URLs. |
| `--s3-bucket NAME` | required | Existing bucket that stores chunks. |
| `--s3-region REGION` | required | Region used when signing S3 requests. |
| `--oidc-issuer URL` | required | Expected token issuer and OIDC discovery base URL. |
| `--oidc-audience AUDIENCE` | required | Required JWT audience. |
| `--retain-versions COUNT` | `16` | Newest versions retained for each file, including its head. |
| `--retain-days DAYS` | `30` | Also retain versions younger than this age. |
| `--gc-every SECONDS` | `600` | Collection interval. Must be at least 1. |
| `--gc-grace SECONDS` | `3600` | Age required before an unreferenced chunk can be deleted. |
| `--grant-ttl SECONDS` | `60` | Lifetime of a presigned chunk URL. |

At startup, `--gc-grace` must be at least twice `--grant-ttl` and at least `--gc-every` plus `--grant-ttl`.

The authority requires `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. `RUST_LOG` controls tracing; the default is `info`.

## `herdfs-agents`

```text
herdfs-agents --mount DIR --host NAME --command COMMAND [OPTIONS]
```

| Flag | Default | Meaning |
|---|---:|---|
| `--mount DIR` | required | Root of the herdfs mount. Any directory can stand in for a mount. |
| `--host NAME` | required | Host directory below `.agents`. It must be one non-empty path component other than `.` or `..`. |
| `--command COMMAND` | required | Shell command run as `sh -c COMMAND` for each session, with the prompt on standard input. |
| `--max-sessions COUNT` | `2` | Maximum concurrent sessions. Must be at least 1. |
| `--poll-ms MILLISECONDS` | `1000` | Delay between directory polls. Must be at least 1. |

For each child, the runner sets `HERDFS_MOUNT` to the canonical mount path and `HERDFS_SESSION_DIR` to the session directory.

## Mount statistics

The mount root exposes process-lifetime JSON counters through `user.herdfs.stats`:

```sh
getfattr --only-values -n user.herdfs.stats MOUNTPOINT
```

The object contains `bytes_fetched`, `chunks_fetched`, `cache_hits`, `grants`, `commits`, `pending_commits`, `round_trip_ms`, `jev_answers`, `jev_unsure`, `jev_errors`, and `decisions`. `decisions` has separate counts for every prefetch, read-ahead and keep value. `round_trip_ms` is null until an authority call has been measured. The xattr exists only on the mount root.
