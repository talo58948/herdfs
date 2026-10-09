# Provenance

This directory vendors `nfsserve` 0.11.0 (BSD-3-Clause) from
<https://github.com/huggingface/nfsserve> at upstream commit
`af4f1176dd709847f3dda4f1a7cd093edaadde63`. `LICENSE` is the unmodified
upstream license.

HerdFS carries these local changes required by its loopback NFSv3 adapter:

- pass requested WRITE stability to the adapter;
- dispatch COMMIT, return an adapter-owned per-incarnation WRITE verifier, and
  allow the adapter to seal and wait for a whole-file generation;
- pass READDIR cookies through both directory APIs;
- deserialize and pass the EXCLUSIVE CREATE verifier, preserve GUARDED create,
  and pass MKDIR attributes;
- distinguish REMOVE from RMDIR;
- use the existing adapter-defined `id_to_fh`/`fh_to_id` hooks for stable
  handles instead of the default process-generation handles;
- retain and replay successful duplicate-XID replies for non-idempotent calls,
  while duplicate requests already in progress wait for that first reply; key
  them by client IP rather than its ephemeral TCP port so reconnects replay;
- serialize WRITE, COMMIT and SETATTR per file handle without blocking reads or
  namespace calls for that handle or any other;
- advertise AUTH_UNIX alone for the loopback export and the POSIX 255-byte
  name limit used by HerdFS, and reject other RPC credential flavors.
