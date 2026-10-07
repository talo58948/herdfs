# Command reference

## `herdfs`

`herdfs` has one operational subcommand, `mount`:

```text
herdfs mount [OPTIONS] --authority URL --space NAME --token-file PATH \
  --cache DIR --cache-bytes BYTES --journal DIR MOUNTPOINT
```

The generated `help` subcommand accepts `herdfs help` and `herdfs help mount`. The top-level command supports `-h`, `--help`, `-V` and `--version`. The `mount` subcommand supports `-h`, `--help` and these arguments:

| Argument | Default | Meaning |
|---|---:|---|
| `--authority URL` | required | Authority base URL. A trailing slash is accepted. |
| `--space NAME` | required | Space to mount. |
| `--token-file PATH` | required | File containing the bearer JWT. It is read at startup and re-read after an HTTP 401 before one retry. |
| `--cache DIR` | required | On-disk verified chunk-cache directory. It is created if absent. |
| `--cache-bytes BYTES` | required | LRU cache limit in bytes. Pinned chunks may exceed it. |
| `--journal DIR` | required | Directory for writes not yet committed. |
| `--strategy VALUE` | `rule` | `stream`, `fetch`, `pin`, `rule`, `chaos:SEED`, or `jev`. |
| `--jev-key-file PATH` | none | File containing the Vercel AI Gateway bearer key. Required when the strategy is `jev`. |
| `--jev-url URL` | `https://ai-gateway.vercel.sh/v1/evaluate` | Endpoint receiving Jev evaluation requests. |
| `--resolve HOST=IP` | none | Connect to the IP for that host while retaining the URL port and host. Repeatable. |
| `MOUNTPOINT` | required | Existing directory where FUSE mounts the space. |

The mount runs until unmounted or until it receives SIGINT or SIGTERM. `RUST_LOG` controls its tracing filter; the default is `info`.

## `herdfs-authority`

```text
herdfs-authority [OPTIONS] --listen ADDRESS --db PATH \
  --s3-endpoint URL --s3-public-endpoint URL --s3-bucket NAME \
  --s3-region REGION --oidc-issuer URL --oidc-audience AUDIENCE
```

The command supports `-h`, `--help` and these flags:

| Flag | Default | Meaning |
|---|---:|---|
| `--listen ADDRESS` | required | Socket address on which the HTTP server listens. |
| `--db PATH` | required | SQLite metadata database, created if absent. |
| `--s3-endpoint URL` | required | S3-compatible endpoint where the authority writes chunks. |
| `--s3-public-endpoint URL` | required | Endpoint placed in presigned chunk-download URLs for clients. |
| `--s3-bucket NAME` | required | Existing bucket that stores chunks. |
| `--s3-region REGION` | required | Region used when signing S3 requests. |
| `--oidc-issuer URL` | required | Expected token issuer and base URL for OIDC discovery. |
| `--oidc-audience AUDIENCE` | required | Required JWT audience. |
| `--retain-versions COUNT` | `16` | Newest versions retained for each file, including its head. |
| `--retain-days DAYS` | `30` | Also retain versions younger than this age. |
| `--gc-every SECONDS` | `600` | Collection interval. Must be at least 1. |
| `--gc-grace SECONDS` | `3600` | Age required before an unreferenced chunk can be deleted. |
| `--grant-ttl SECONDS` | `60` | Lifetime of a presigned chunk URL. |

At startup, `--gc-grace` must be at least twice `--grant-ttl` and at least `--gc-every` plus `--grant-ttl`.

The authority requires `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. `RUST_LOG` controls its tracing filter; the default is `info`.

## `herdfs-agents`

```text
herdfs-agents --mount DIR --host NAME --command COMMAND [OPTIONS]
```

The command supports `-h`, `--help` and these flags:

| Flag | Default | Meaning |
|---|---:|---|
| `--mount DIR` | required | Root of the herdfs mount. Any directory can stand in for a mount. |
| `--host NAME` | required | Host directory below `.agents`. It must be one non-empty path component other than `.` or `..`. |
| `--command COMMAND` | required | Shell command run as `sh -c COMMAND` for each session, with the prompt on standard input. |
| `--max-sessions COUNT` | `2` | Maximum concurrent sessions. Must be at least 1. |
| `--poll-ms MILLISECONDS` | `1000` | Delay between directory polls. Must be at least 1. |

For each child command, the runner sets `HERDFS_MOUNT` to the canonical mount path and `HERDFS_SESSION_DIR` to the session directory.

## Mount statistics

The mount root exposes process-lifetime JSON counters through `user.herdfs.stats`:

```sh
getfattr --only-values -n user.herdfs.stats MOUNTPOINT
```

The object contains `bytes_fetched`, `chunks_fetched`, `cache_hits`, `grants`, `commits`, `pending_commits`, `round_trip_ms`, `jev_answers`, `jev_unsure`, `jev_errors`, and `decisions`. `decisions` contains counts for `stream`, `fetch` and `pin`; `round_trip_ms` is null until an authority call has been measured. The xattr exists only on the mount root.
