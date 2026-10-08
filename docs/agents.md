# Agents as files

`herdfs-agents` is a separate runner that watches one host directory in a mounted space. A session request is a directory and ordinary files; the runner uses no private herdfs API.

## Layout

```text
/.agents/HOST/SESSION/
├── prompt
├── cwd
├── status
└── output
```

- `prompt` is required and must be a non-empty regular file. It becomes the command's standard input after it is unchanged across two polls.
- `cwd` is optional. It names a directory relative to the mount; without it, the command runs at the mount root. Absolute paths, parent traversal and symlink traversal are rejected.
- `status` is written by the runner. Its values are `running`, `done`, or `failed: REASON`. A runner restart changes sessions it finds in `running` to `failed: runner restarted`.
- `output` receives both standard output and standard error, appended in their shared file. The runner syncs it before writing a final status.

The child command also receives `HERDFS_MOUNT` and `HERDFS_SESSION_DIR`.

## Permissions

[A1](../CONTRACT.md) makes filesystem permission the spawn permission. Write access on one session directory permits a caller to supply that session's prompt but not create another session. Write access on `/.agents/HOST/` permits creating any session below that host. An ancestor ACL can provide the same effective permission.

The host directory and any pre-authorized session directories must be created by a principal that already has write access. ACLs are set through the authority's authenticated API as shown in the [quickstart](quickstart.md#create-a-space-and-grant-access).

## Run a host

This proven example echoes the prompt and adds one line:

```sh
./target/release/herdfs-agents \
  --mount /mnt/herdfs \
  --host build-1 \
  --command 'cat; echo ok'
```

Assuming `/mnt/herdfs/.agents/build-1` already exists and the requester can write there, request a session from any mounted client:

```sh
SESSION=/mnt/herdfs/.agents/build-1/demo-001
mkdir "$SESSION"
printf 'hello\n' > "$SESSION/prompt"

until [ -f "$SESSION/status" ] && [ "$(cat "$SESSION/status")" != running ]; do
  sleep 1
done
cat "$SESSION/status"
cat "$SESSION/output"
```

The runner polls every second by default, marks the session `running`, and runs the command with the prompt on standard input. When complete, `status` is `done` and `output` contains:

```text
hello
ok
```

The runner handles at most two sessions concurrently by default and waits for running sessions to finish when it receives SIGINT or SIGTERM. See the [command reference](cli.md#herdfs-agents) for every flag.
