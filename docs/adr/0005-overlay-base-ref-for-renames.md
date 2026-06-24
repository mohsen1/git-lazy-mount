# 0005 — Overlay base-refs make clean renames fetch-free

**Status:** Accepted

## Context

Under a `blob:none` partial clone, a file's content may not be present locally.
Renaming a **clean** (unmodified) file should not require fetching its bytes — the
content is unchanged and already identified by the Git blob it came from. Storing a
full copy in the overlay on every rename would defeat lazy hydration and waste
space and network.

## Decision

Give the overlay a dedicated entry kind,
[`OverlayKind::BaseRef { oid, mode }`](../../crates/overlay/src/lib.rs), that
**references an existing Git blob without storing any bytes**.
A clean-file (or clean-subtree) rename writes a base-ref at the destination path:
no content is materialized and **no blob fetch occurs**. Reads of a base-ref
resolve the referenced blob lazily through the object provider; `stat` size also
comes from the object, so the rename touches no content path.

This is distinct from the inode table preserving identity across a rename: the
inode table keeps the *handle* valid, while the base-ref keeps the *content*
fetch-free.

## Consequences

* A clean rename is `O(1)` metadata, offline-capable, and writes zero content
  bytes — verified by `overlay::base_ref_stores_no_content` (no content file is
  created; `(oid, mode)` round-trips across reopen) and the workspace rename
  tests.
* The overlay must resolve base-ref reads through the provider, so a base-ref to a
  still-missing blob hydrates on first read under a network-permitted policy (and
  errors cleanly offline).
* If the destination is later edited, copy-on-write replaces the base-ref with
  stored content as usual.
