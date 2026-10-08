# Access plans and Jev

herdfs chooses an access plan for each read-only access. The plan changes when chunks are fetched and how long they resist eviction. It never changes file bytes or bypasses permissions. Every plan reads through the same verified chunk cache, as required by [C8, P1, P2 and P3](../CONTRACT.md).

Linux starts an access when a file is opened. macOS support is in progress.

## Plan settings

Each plan sets three things independently:

| Setting | Values | Effect |
|---|---|---|
| Prefetch | `none`, `first`, `index`, `whole` | Fetch nothing, the first two chunks, the format's index regions, or the whole file when access starts. |
| Read-ahead | `none`, `small`, `large` | After sequential reads begin, fetch no extra chunks, the next two chunks, or the next eight chunks. |
| Keep | `no`, `yes` | Use normal cache eviction, or protect this version's cached chunks from eviction until the access ends. |

Prefetch and read-ahead run in the background. Reads do not wait for either to finish. `keep=yes` does not fetch bytes by itself. Kept chunks for an access may take the cache over `--cache-bytes`; they become eligible for eviction when the access ends.

## Named and explicit plans

The three named plans remain as short forms:

| Name | Prefetch | Read-ahead | Keep |
|---|---|---|---|
| `stream` | `none` | `none` | `no` |
| `fetch` | `whole` | `none` | `no` |
| `pin` | `whole` | `none` | `yes` |

An explicit plan uses `PREFETCH/READ_AHEAD/KEEP` form. For example:

```sh
herdfs mount --strategy index/small/yes ...
```

The values are lowercase and follow the tables above.

## Local deciders

A named or explicit plan applies the same settings to every read-only access.

The default `rule` uses `whole/none/no` for files smaller than 64 MiB and `none/none/no` for files of 64 MiB or larger. This is also Jev's fallback.

`chaos:SEED` chooses each setting independently. The seed makes the sequence reproducible. Chaos exists to exercise plan-independent behavior.

## Index discovery

`prefetch=index` computes byte ranges in the client. Jev never supplies ranges.

- Parquet uses the footer metadata when the leading and trailing `PAR1` markers are valid.
- ZIP uses the range from the central directory to the end of the file.
- MP4 uses a `moov` box found among the first four top-level boxes, beginning with an `ftyp` box.
- Unknown or malformed input falls back to the first and last two chunks.

The probe reads bounded headers and tails. The resulting chunks use the ordinary grant, hash verification and cache path.

## Jev

Jev uses the `typesafe-ai/jev` model. It asks three named `choice` questions in one request: `prefetch`, `read_ahead` and `keep`. Each question offers only that setting's valid values.

The request describes the access with these fields:

| Field | Value sent |
|---|---|
| `program` | The opening process name. |
| `opened by this program` | First access, once before, or several times in the last five minutes. |
| `file extension` | The final path component's extension, or `none`. |
| `file size` | A coarse decimal-megabyte magnitude. |
| `already cached` | `none of it`, `some of it`, `most of it`, or `all of it`. |
| `network round trip` | A coarse authority round-trip magnitude. |
| `time to download the whole file` | A coarse estimate from measured throughput, or 10 MB/s before a measurement. |

### Non-blocking decisions

Access never waits for Jev.

- A remembered confident value applies immediately for its setting.
- Without one, that setting uses the rule immediately while one request runs for the complete phrased context.
- Confidence of at least `0.3` is accepted. A missing, malformed or lower-confidence answer falls back to the rule for that setting only.
- The request times out after 2 seconds. Transport, HTTP, invalid JSON and timeout errors leave all three rule values in place. A later matching access may retry after a request error.
- A late confident answer affects only an access that is still open. It may add prefetch, read-ahead or retention, but cannot reduce work already selected.

This is the availability boundary in P1 and P2. Model latency and failure do not block access.

## Explain the latest access

`herdfs explain` reads the latest access record from a mounted file:

```sh
herdfs explain /mnt/demo/video.mp4
```

It prints JSON with the three settings, the source (`fixed`, `chaos`, `rule`, `jev` or `remembered`), Jev's answer when present, and bytes fetched for that access. It fails if the file has no recorded access since the mount started.

## Enabling Jev

On Linux, mount with:

```sh
herdfs mount \
  --authority http://127.0.0.1:7000 \
  --space demo \
  --cache /tmp/herdfs-demo/cache \
  --cache-bytes 1073741824 \
  --journal /tmp/herdfs-demo/journal \
  --strategy jev \
  --jev-key-file /path/to/ai-gateway-key \
  /tmp/herdfs-demo/mnt
```

macOS support is in progress. `--jev-key-file` is required with `--strategy jev`; its trimmed contents are sent as a bearer key. `--jev-url` defaults to `https://ai-gateway.vercel.sh/v1/evaluate`.
