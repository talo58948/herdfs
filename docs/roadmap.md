# Roadmap

Planned, not built. Nothing here is implemented or tested.

## Native macOS client (FSKit)

Today the macOS client serves a space to the system's NFS client on loopback (CONTRACT M1, M2). That needs no Apple account, but it costs three things:

- **Other local users.** macOS lets any process bind a reserved port, so another account on the same Mac can mount and use the signed-in user's drive. The NFS client is for single-user Macs.
- **Open and close.** NFS shows neither, so versions switch per access epoch instead of between reads (M2), and access plans see no process name.
- **Saves.** Every save waits for one authority commit (about 0.14 s per app save on an M4 Pro), and a large write may become visible as several committed prefixes.

A native FSKit file-system extension removes all three: the kernel delivers open, read, write and close with the caller's identity, and the volume is an ordinary local mount. FSKit needs the paid Apple Developer Program for its entitlement and for notarization, so it waits until that membership exists.

The plan, when it does:

- A Swift FSKit module over the client's shared `Space` core (the same core the FUSE and NFS adapters use), through a small C ABI. The NFS client stays as the fallback for unsigned builds.
- M1 and M2 shrink to "on macOS without the FSKit extension"; with it, C1–C15 and P1 hold as on Linux.
- Verification: the full lab over FSKit on a macOS CI host, plus the Finder checklist in `macOS test tooling`.

Tracked in the repository's issues as "Native macOS client (FSKit)".
