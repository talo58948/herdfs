# Quickstart

This is the shortest path from a source checkout to one mounted space. Start the store and authority, sign in, create a space, then mount it.

For complete local-issuer, generic OIDC and Clerk configurations, see [Self-hosting](self-hosting.md).

## Prerequisites

- Linux with FUSE support, access to `/dev/fuse`, and `fusermount3` for unmounting. macOS is supported through the [menu-bar app](architecture.md#macos-loopback-nfs) on single-user Macs.
- A Rust toolchain with Cargo.
- An existing bucket in an S3-compatible store. You need its internal endpoint, the endpoint clients can reach, region, access key and secret key.
- An OIDC issuer with discovery and JWKS documents. Its signed JWTs must contain `iss`, `sub`, `aud` and `exp`; `aud` must match `--oidc-audience`. An optional `groups` array supplies group principals.
- A public OIDC client for `herdfs login`. It must allow authorization-code login with PKCE, loopback callbacks and the `openid offline_access` scopes.
- A browser on the Linux machine for login.

herdfs does not create the bucket or configure the identity provider.

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

## Sign in and create a space

Sign in as the user who will mount the space. The command opens the issuer in a browser and saves renewable tokens at `~/.config/herdfs/token`:

```sh
./target/release/herdfs login \
  --issuer https://issuer.example \
  --client-id herdfs-cli
```

Create `demo`. Its creator receives `admin` on the root automatically:

```sh
./target/release/herdfs create-space \
  --authority http://127.0.0.1:7000 \
  demo
```

Use `herdfs share` to grant another `user:` or `group:` principal access. Permissions are `read`, `write` and `admin`.

## Mount and use the space

In another terminal, prepare local state and start the mount. It uses the saved login token by default:

```sh
STATE=/tmp/herdfs-demo
mkdir -p "$STATE/cache" "$STATE/journal" "$STATE/mnt"

./target/release/herdfs mount \
  --authority http://127.0.0.1:7000 \
  --space demo \
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

The default access decider is `rule`. See [Access plans and Jev](strategies.md) for alternatives and [the command reference](cli.md) for all commands and flags.
