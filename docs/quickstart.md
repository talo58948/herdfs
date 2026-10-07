# Quickstart

This is the shortest path from a source checkout to one mounted space. The lab follows the same sequence: start the store, issuer and authority; create a space; set its ACL; then start a client.

## Prerequisites

- Linux with FUSE support, access to `/dev/fuse`, and `fusermount3` for unmounting.
- A Rust toolchain with Cargo.
- An existing bucket in an S3-compatible store. You need its internal endpoint, the endpoint clients can reach, region, access key and secret key.
- An OIDC issuer with a discovery document and JWKS. Its signed JWTs must contain `iss`, `sub`, `aud` and `exp`; `aud` must match `--oidc-audience`. An optional `groups` array supplies group principals.
- `curl` for the two setup RPCs below.

herdfs does not create the bucket or issue login tokens.

## Build

From the repository root:

```sh
cargo build --release
```

This builds:

- `target/release/herdfs`
- `target/release/herdfs-authority`
- `target/release/herdfs-agents`

## Start the authority

Set the authority's object-store credentials, replace the example endpoints and issuer, then keep this process running:

```sh
export AWS_ACCESS_KEY_ID='your-access-key'
export AWS_SECRET_ACCESS_KEY='your-secret-key'

./target/release/herdfs-authority \
  --listen 127.0.0.1:7000 \
  --db ./herdfs.db \
  --s3-endpoint http://127.0.0.1:9000 \
  --s3-public-endpoint http://127.0.0.1:9000 \
  --s3-bucket herdfs \
  --s3-region us-east-1 \
  --oidc-issuer https://issuer.example \
  --oidc-audience herdfs
```

`--s3-public-endpoint` is placed in presigned download URLs, so every client must be able to reach it. A healthy authority answers at `http://127.0.0.1:7000/v1/health`.

## Create a space and grant access

There is no shipped administration CLI. Space creation and ACL changes use the authenticated JSON API. Put any valid creator JWT in `/path/to/creator.jwt` and a JWT whose `sub` is `alice` in `/path/to/alice.jwt`.

Create `demo`; its creator receives `admin` on the root automatically:

```sh
CREATOR_TOKEN="$(tr -d '\n' < /path/to/creator.jwt)"
curl -fsS http://127.0.0.1:7000/v1/rpc \
  -H "Authorization: Bearer $CREATOR_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"op":"create_space","space":"demo"}'
```

Grant Alice write access at the root. Root is encoded as the empty path:

```sh
curl -fsS http://127.0.0.1:7000/v1/rpc \
  -H "Authorization: Bearer $CREATOR_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"op":"set_acl","space":"demo","path":"","principal":"user:alice","perm":"write"}'
```

Permissions are `read`, `write` and `admin`. A principal is `user:` followed by the token subject, or `group:` followed by a name from the token's `groups` claim.

## Mount and use the space

In another terminal, prepare local state and copy Alice's raw JWT into the token file:

```sh
STATE=/tmp/herdfs-demo
mkdir -p "$STATE/cache" "$STATE/journal" "$STATE/mnt"
cp /path/to/alice.jwt "$STATE/token"
chmod 600 "$STATE/token"

./target/release/herdfs mount \
  --authority http://127.0.0.1:7000 \
  --space demo \
  --token-file "$STATE/token" \
  --cache "$STATE/cache" \
  --cache-bytes 1073741824 \
  --journal "$STATE/journal" \
  "$STATE/mnt"
```

The process stays in the foreground while the mount is active. From another shell:

```sh
printf 'hello from herdfs\n' > /tmp/herdfs-demo/mnt/hello.txt
cat /tmp/herdfs-demo/mnt/hello.txt
fusermount3 -u /tmp/herdfs-demo/mnt
```

The default access strategy is `rule`. See [Access strategies and Jev](strategies.md) for the alternatives and [the command reference](cli.md) for all flags.
