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

Start the authority with `--local-issuer` as above. Its `--listen` port must be fixed and nonzero. The issuer URL is tied to that address; changing `--listen` requires deleting the key file and reissuing every token. It creates `herdfs.db.local-issuer.json` with mode 0600. In another shell, issue one token per machine:

```sh
(umask 077; ./target/release/herdfs-authority issue-token \
  --db ./herdfs.db --user alice --days 30 > ./alice.token)
```

The command writes the token ID to standard error. Revoke that machine immediately with:

```sh
./target/release/herdfs-authority revoke-token \
  --db ./herdfs.db TOKEN_ID
```

The key file can mint tokens for any user, so protect it and back it up with the database; the authority refuses a key file that others can access. Deleting it invalidates every token.

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

Create a public OAuth application in Clerk with PKCE and set:

```text
Redirect URI: http://127.0.0.1:53682/callback
Scopes: openid offline_access
Access token format: JWT
Include audience: On
```

Clerk includes `aud` only when the authorization request, code exchange and every refresh carry the same RFC 8707 `resource`, which must be an absolute URI. Run the authority and log in with the same URI:

```sh
./target/release/herdfs-authority \
  --oidc-issuer https://<instance>.clerk.accounts.dev \
  --oidc-audience https://your-domain.example

./target/release/herdfs login \
  --issuer https://<instance>.clerk.accounts.dev \
  --client-id <public-client-id> \
  --resource https://your-domain.example
```

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
