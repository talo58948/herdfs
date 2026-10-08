# Self-hosting

herdfs needs one S3-compatible bucket, one authority process and a client on each machine. The authority alone receives the bucket credentials.

## Store and authority

Use an existing S3 bucket, or run MinIO locally and create an `herdfs` bucket. Set credentials for that bucket, then start the authority:

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
  --local-issuer
```

`--s3-public-endpoint` must be reachable by every client because it appears in presigned download URLs.

## Identity

Choose one mode.

### Built-in local issuer

Start the authority with `--local-issuer` as above. It creates `herdfs.db.local-issuer.json` with mode 0600. In another shell, issue one token per machine:

```sh
./target/release/herdfs-authority issue-token \
  --db ./herdfs.db --user alice --days 30 > ./alice.token
```

The command writes the token ID to standard error. Revoke that machine immediately with:

```sh
./target/release/herdfs-authority revoke-token \
  --db ./herdfs.db TOKEN_ID
```

### Any OIDC provider

Replace `--local-issuer` with:

```sh
--oidc-issuer https://issuer.example --oidc-audience herdfs
```

Configure a public authorization-code client with PKCE, exact redirect URI `http://127.0.0.1:53682/callback`, and scopes `openid offline_access`. The access token must be a JWT whose `iss` and `aud` match the authority flags. Sign in with:

```sh
./target/release/herdfs login \
  --issuer https://issuer.example \
  --client-id herdfs-cli
```

Use `--redirect-port PORT` only when that exact URI is also registered; `--redirect-port 0` restores a random loopback port.

### Clerk hosted example

Create a Clerk public client with PKCE and set these values in the Clerk application:

```text
Redirect URI: http://127.0.0.1:53682/callback
Scopes: openid offline_access
Issuer: https://<clerk-issuer>
Client ID: <clerk-public-client-id>
Audience: <clerk-jwt-audience>
```

Run the authority with `--oidc-issuer https://<clerk-issuer> --oidc-audience <clerk-jwt-audience>`, then pass the issuer and client ID to `herdfs login` as in the generic example.

## Create and mount

For local mode, pass the issued token explicitly. OIDC login uses its saved token file by default.

```sh
./target/release/herdfs create-space \
  --authority http://127.0.0.1:7000 \
  --token-file ./alice.token demo

mkdir -p ./state/cache ./state/journal ./mnt
./target/release/herdfs mount \
  --authority http://127.0.0.1:7000 \
  --space demo \
  --token-file ./alice.token \
  --cache ./state/cache \
  --cache-bytes 1073741824 \
  --journal ./state/journal \
  ./mnt
```
