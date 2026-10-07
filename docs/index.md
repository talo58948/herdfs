# herdfs

herdfs is one shared filesystem for Linux boxes, backed by any S3-compatible bucket.

## Three runtimes

- The object store holds chunk bytes. It is private to the authority.
- `herdfs-authority` owns namespaces, versions, ACLs and grants. It is the only runtime with object-store credentials.
- `herdfs mount` presents one space to one Linux principal through FUSE. It caches reads and journals writes locally.

Separate plugins, including `herdfs-agents`, use the mounted filesystem like any other program. See the [architecture](architecture.md) for the complete design.

## v0 scope

v0 includes shared files and directories, OIDC identity, path ACLs, immutable file versions, retention and collection, space forks, offline content writes to existing files, per-open access strategies, and the agents-as-files plugin. The [contract](../CONTRACT.md) defines the exact promises.

Hard links, macOS and Windows clients, cross-host file locking, coherent shared-writable mappings, and the other explicit exclusions are [out of v0](architecture.md#deliberately-out-of-v0).

## Next

- Follow the [quickstart](quickstart.md) to build and mount a space from source.
- Read [Access strategies and Jev](strategies.md) before choosing a mount strategy.
- Use the [agents-as-files plugin](agents.md) to run sessions requested through files.
- Consult the [command reference](cli.md) for every shipped flag.
