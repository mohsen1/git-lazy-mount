# Architecture

git-lazy-mount is **not** "a partial clone with a FUSE wrapper." It is a durable
working-copy engine that uses Git as its persistent commit and transport format.
Five layers compose it:

1. **Git object database and refs** — the external source of truth, accessed via
   the installed `git` binary (`glm-git-store`). Git owns the network protocol,
   credentials, object format, ref transactions, filters, fetch, and push.
2. **Transactional workspace state** — immutable views advanced through an
   append-only operation log (`glm-oplog`, `glm-workspace`).
3. **Writable copy-on-write overlay** — locally materialized content, tombstones,
   and base-references (`glm-overlay`), plus a separate staged delta
   (`glm-stage`).
4. **Virtual filesystem projection** — backend logic shared across platforms
   (`glm-fs-common`, `glm-fs-fuse`/`fskit`/`projfs`), driven by the daemon.
5. **Git interoperability adapter** — native commands are authoritative;
   ordinary Git commands are supported only in proven tiers (see
   [git-compatibility.md](git-compatibility.md)).

## How this differs from sparse checkout and partial clone

* **Sparse checkout** still writes real files for the included set and tracks
  state in `.git/index`; the excluded set is simply absent from the tree you
  see. git-lazy-mount shows the **entire** logical tree and materializes content
  per-file on demand, with a transactional model that is not constrained to the
  shape of `.git/index`.
* **Partial clone** is an object-availability optimization: objects are fetched
  lazily by Git itself, but a normal checkout still materializes the whole
  working set on disk and `git status` still scans it. git-lazy-mount uses
  partial clone as its object substrate but adds the writable overlay, the
  transactional views, `O(changed paths)` status, and the filesystem projection
  on top — and it routes *all* missing-object access through an explicit
  provider so a filesystem read never silently triggers a credential prompt.

## Separation of concerns (the orthogonal state model)

The spec is emphatic: do not collapse distinct conditions into one `hydrated`
boolean. `glm-core` defines four independent axes
([state-model.md](state-model.md)):

* **Source** — base tree / overlay / tombstone / conflict / gitlink / native
  redirection.
* **Semantics** — clean / modified / new / deleted / mode-changed / type-changed
  / renamed / copied / conflicted.
* **Residency** — how much is locally present (tree metadata, raw blob, filtered
  content, inode, OS placeholder, overlay bytes), each independent.
* **Durability** — in-memory < journaled < data-fsynced < metadata-committed <
  operation-sealed.

A file can be fully **materialized** (overlay bytes present) yet byte-for-byte
**clean**; a **rename** can be semantically modified while its content is never
fetched (a base-reference). These are different axes by construction.

## Shared store, independent workspaces

A repository is cloned once into a **bare** shared store keyed by a
credential-free repository identity (`glm-platform::repo_id`; `https://…/o/r`
and `git@…:o/r` map to the same store). Multiple mounts share the object
storage, packfiles, and fetch machinery, but each has an independent base
revision, **private workspace ref** (`refs/lazy-mount/workspaces/<id>/head`,
which protects unpushed commits from GC), attached-branch lease, stage, overlay,
operation log, and inode map. One workspace cannot move or corrupt another.

## Why a transactional workspace is needed

Every semantic mutation produces a new immutable view identifying the base
commit, the workspace-private head, the attached branch and its **expected**
value (for compare-and-swap), the stage/overlay/conflict roots, the path-mapping
and filter-context versions, and the mount generation. Views are advanced
through an append-only log whose current pointer is updated **only after** all
referenced records are durable. This is what makes uncommitted work survive
crashes and makes "desired vs applied" generation skew *detectable* rather than
silently corrupting state. See [operation-log.md](operation-log.md) and
[failure-recovery.md](failure-recovery.md).

## Read and write paths

* **Read** (`workspace.read_file`): resolve the path in view order
  (overlay → base-ref → base); for a base blob, ensure the object is present
  (the provider coalesces concurrent fetches and enforces the fetch policy),
  then apply Git's working-tree filters using `--attr-source=<base>`, and stream
  the requested range. Reads never mark a file modified.
* **Write** (`workspace.write_*`): copy-on-write into the overlay.
  `O_TRUNC`/create does not fetch the old content; partial overwrite
  materializes the base once and preserves untouched bytes; delete writes a
  tombstone; a clean rename writes a base-reference (no blob fetch).

## Commit and push

`commit` builds the new tree from `base ⊕ staged-delta`, **reusing unchanged
subtrees** (`O(changed regions)`), writes an ordinary Git commit, advances the
private head ref unconditionally, then advances the attached branch via
compare-and-swap. A concurrently moved branch is reported as divergence — the
workspace commit stays reachable on its private ref and is never lost.
`push` uses `--force-with-lease` against the last-known remote value so a
concurrent remote update is detected, not clobbered. Pushes are modeled as
retryable saga steps, not part of the local atomic transaction.

## Crate dependency sketch

```
core ◄── platform, git-store, overlay, stage, oplog, filters, fsmonitor, ipc,
         projection, metadata
git-store ◄── object-provider ◄── workspace
overlay, stage, oplog ◄── workspace
workspace ◄── fs-common ◄── fs-fuse / fs-fskit / fs-projfs
workspace, platform, object-provider ◄── daemon ◄── cli
```

Platform-specific FFI and `unsafe` are isolated to the per-backend crates; every
other crate is `#![forbid(unsafe_code)]`.
