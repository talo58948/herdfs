# Access strategies and Jev

herdfs chooses a materialization strategy for each read-only file open. The choice changes when chunks are fetched and retained; it never changes file bytes or bypasses permissions. Every strategy reads the same committed version through the verified chunk cache, as required by [C8, P1 and P2](../CONTRACT.md).

## Strategies

| Strategy | At open | Eviction |
|---|---|---|
| `stream` | Starts no prefetch. Reads fetch only missing chunks that overlap the requested range. | Cached chunks are subject to the cache's least-recently-used limit. |
| `fetch` | Starts a background prefetch of every chunk. Reads can proceed while it runs. | Cached chunks are subject to the least-recently-used limit. |
| `pin` | Pins every chunk for that handle and starts a background prefetch of every chunk. | Pinned chunks are not evicted while the handle is open and may exceed `--cache-bytes`. They become eligible after close. |

A pinned handle follows the file when the client learns of a newer version, moving its pins to that version's chunks.

## Local deciders

Fixed deciders select `stream`, `fetch` or `pin` for every read-only open.

The `rule` decider selects `fetch` below exactly 64 MiB and `stream` at 64 MiB or above. It is the default and Jev's fallback.

`chaos:SEED` makes a reproducible seeded random choice among all three strategies on each open. It exists to exercise strategy-independent behavior.

## Jev

Jev is TypeSafe's System One model, named `typesafe-ai/jev` in the request. It chooses one of `stream`, `fetch` or `pin` from the context of each read-only open.

The request's `state` contains exactly these fields:

| Field | Value sent |
|---|---|
| `program` | The opening process name read from `/proc/PID/comm`. |
| `opened by this program` | `for the first time`, `once before`, or `several times in the last few minutes`. Repeats are counted for the same file entry and process name within five minutes. |
| `file extension` | The final path component's extension, or `none`. |
| `file size` | Decimal megabytes phrased as `under a megabyte`, `a few megabytes`, `tens of megabytes`, `hundreds of megabytes`, or `thousands of megabytes`. |
| `already cached` | After truncating the cached percentage: `none of it` at 0, `some of it` from 1 through 49, `most of it` from 50 through 98, and `all of it` from 99 through 100. |
| `network round trip` | Authority round-trip time phrased as `under a millisecond`, `a few milliseconds`, `tens of milliseconds`, `hundreds of milliseconds`, or `thousands of milliseconds`. |
| `time to download the whole file` | File size divided by recent measured throughput, phrased as `under a second`, `a few seconds`, `tens of seconds`, `hundreds of seconds`, or `thousands of seconds`. Before a measurement it assumes 10,000,000 bytes per second. |

The typed question is a `choice` named `strategy`: "How should this file be read so that the program finishes soonest?" Its criteria are:

- `stream`: "Download only the parts the program reads, as it reads them. Best when it reads a small part of a large file."
- `fetch`: "Download the whole file in the background right away. Best when it reads most of the file once."
- `pin`: "Download the whole file and keep it cached. Best when it reads the file repeatedly or must keep it available."

### Non-blocking decisions

`open` never waits for the model. Jev remembers answers by the complete phrased context for the life of the mount process:

- A remembered confident answer applies immediately.
- On a miss, the size rule applies immediately and one background request starts for that context.
- Confidence of at least `0.3` is accepted. A lower-confidence answer is remembered as unsure, so the rule continues to apply.
- The request times out after 2 seconds. Transport, HTTP, response-shape and timeout errors leave the rule in place; a later matching open may retry.
- If the rule began with `stream`, a later confident `fetch` or `pin` answer starts that strategy only if the handle is still open.

This is the availability boundary in P1 and P2: model latency and failure do not block access.

## Enabling Jev

Mount with:

```sh
./target/release/herdfs mount \
  --authority http://127.0.0.1:7000 \
  --space demo \
  --token-file /tmp/herdfs-demo/token \
  --cache /tmp/herdfs-demo/cache \
  --cache-bytes 1073741824 \
  --journal /tmp/herdfs-demo/journal \
  --strategy jev \
  --jev-key-file /path/to/ai-gateway-key \
  --jev-url https://ai-gateway.vercel.sh/v1/evaluate \
  /tmp/herdfs-demo/mnt
```

`--jev-key-file` is required with `--strategy jev`; its trimmed contents are sent as a bearer key. `--jev-url` defaults to the URL shown above. The client posts the typed choice request for model `typesafe-ai/jev` to that endpoint.
