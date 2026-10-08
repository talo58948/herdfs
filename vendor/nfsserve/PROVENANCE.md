# Provenance

This directory vendors `nfsserve` 0.11.0 (BSD-3-Clause) from
<https://github.com/huggingface/nfsserve> at upstream commit
`af4f1176dd709847f3dda4f1a7cd093edaadde63`, as recorded by the published
crate's `.cargo_vcs_info.json`. `LICENSE` is the unmodified upstream license.

HerdFS carries these local changes required by its loopback NFSv3 adapter:

- pass requested WRITE stability and listener arrival sequence to the adapter;
- dispatch COMMIT, return an adapter-owned per-incarnation WRITE verifier, and
  allow the adapter to seal and wait for a whole-file generation;
- pass READDIR cookies through both directory APIs;
- deserialize and pass the EXCLUSIVE CREATE verifier, preserve GUARDED create,
  and pass MKDIR attributes;
- distinguish REMOVE from RMDIR;
- use the existing adapter-defined `id_to_fh`/`fh_to_id` hooks for stable
  handles instead of the default process-generation handles;
- retain and replay completed duplicate-XID replies, while duplicate requests
  already in progress wait for that first reply;
- assign sequence numbers before concurrent dispatch and serialize mutations
  in arrival order, so COMMIT cannot overtake an earlier WRITE;
- advertise AUTH_UNIX alone for the loopback export and the POSIX 255-byte
  name limit used by HerdFS; and
- remove pre-existing trailing whitespace from the README and one source file,
  and allow a Clippy lint emitted by current `async-trait` expansion.
