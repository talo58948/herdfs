# herdfs

herdfs is one shared filesystem for Linux boxes, backed by any S3-compatible bucket. macOS support is in progress.

## Three runtimes

- The object store holds chunk bytes. It is private to the authority.
- `herdfs-authority` owns namespaces, versions, ACLs and grants. It is the only runtime with object-store credentials.
- `herdfs mount` presents one space to one Linux principal through FUSE. It caches reads and journals writes locally. macOS support is in progress.

Separate plugins, including `herdfs-agents`, use the mounted filesystem like any other program. See the [architecture](architecture.md) for the complete design.

## v0.1 scope

v0.1 includes shared files and directories, OIDC login, path ACLs, immutable file versions, retention and collection, space forks, offline content writes to existing files, independent per-access plan settings, and the agents-as-files plugin. The [contract](../CONTRACT.md) defines the exact promises.

Hard links, Windows clients, cross-host file locking, coherent shared-writable mappings, and the other explicit exclusions are [out of v0.1](architecture.md#deliberately-out-of-v01). macOS support is in progress.

## Next

- Follow the [quickstart](quickstart.md) to build and mount a space from source.
- Read [Access plans and Jev](strategies.md) before choosing an access plan.
- Use the [agents-as-files plugin](agents.md) to run sessions requested through files.
- Consult the [command reference](cli.md) for every shipped flag.
