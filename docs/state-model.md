# State model

git-lazy-mount refuses to collapse a path's condition into one `hydrated`
boolean. The spec (§2.6, §12) is emphatic on this point, and `glm-core`
(`crates/core/src/state.rs`) gives each concern its own type. A path has a
**source**, a **semantic status**, a **residency**, and a **durability**, and
these four axes are independent by construction.

The classic confusion this prevents: *materialized* is not *modified*.

* A file can be fully resident (overlay bytes present, or its raw blob cached)
  and still be **byte-for-byte clean** — you opened it, the editor read it, and
  nothing changed.
* A **rename** can be semantically `Modified`/`Renamed` while its content is
  **never fetched**: a clean rename records a *base-reference* to the existing
  Git blob (`OverlayKind::BaseRef`), so the path moves with zero residency.

Residency answers "how much is local?"; semantics answers "how does it differ
from the committed form?". Knowing one tells you nothing about the other.

## The four axes

### 1. Source — where the working-tree content comes from

`core::state::Source` (an enum, one variant per path):

| Variant | Meaning |
| --- | --- |
| `Base` | Backed by the committed base tree. |
| `Overlay` | Backed by locally written overlay bytes. |
| `Tombstone` | Deleted in the working tree. |
| `Conflict` | A structured conflict record. |
| `Gitlink` | A submodule gitlink. |
| `NativeRedirect` | An untracked path served from a native-disk redirection. |

This axis records the *origin* of bytes, not their newness. (`Gitlink`,
`Conflict`, and `NativeRedirect` are part of the model; the resolution code in
`glm-workspace` today handles `Base`, `Overlay`, `Tombstone`, and base-ref
renames — see the resolution order below.)

### 2. SemanticStatus — how the path differs from its committed form

`core::state::SemanticStatus`:
`Clean`, `Modified`, `New`, `Deleted`, `ModeChanged`, `TypeChanged`, `Renamed`,
`Copied`, `Conflicted`.

This is the Git-meaning of the path. It is computed, never assumed from
residency. A clean base file and a fully-cached clean base file are both
`Clean`.

### 3. Residency — how much is locally present

`core::state::Residency` is a struct of **six independent booleans**, not a
ladder:

* `tree_metadata_cached` — this directory's tree metadata is cached.
* `raw_blob_cached` — the raw Git blob is cached locally.
* `filtered_content_cached` — the filtered working-tree content is cached.
* `inode_loaded` — an inode has been loaded for this path.
* `os_placeholder_present` — the OS has a placeholder/projection present.
* `overlay_bytes_present` — overlay bytes exist on disk for this path.

The doc comment is explicit: *"not resident" never implies a size of zero and
never implies "clean" or "modified".* The only derived helper is
`Residency::is_materialized()`, which is `raw_blob_cached ||
filtered_content_cached || overlay_bytes_present` — i.e. "any byte content is
present locally." Its own comment warns that this is **not** the same as
modified.

### 4. Durability — how durably a mutation has landed

`core::state::Durability` is an *ordered* enum (`PartialOrd`/`Ord`); a higher
level implies every lower guarantee:

```
InMemory < Journaled < DataFsynced < MetadataCommitted < OperationSealed
```

* `InMemory` — exists only in process memory.
* `Journaled` — appended to the journal, not yet fsynced.
* `DataFsynced` — file data has been fsynced.
* `MetadataCommitted` — state records fsynced and `CURRENT` advanced.
* `OperationSealed` — wrapped into a sealed, immutable operation-log entry.

The operation log advances its current pointer only once the relevant records
reach `MetadataCommitted` (spec §13); see
[operation-log.md](operation-log.md).

## Working-tree resolution order (spec §11)

The working tree is resolved per path, in this strict order (implemented in
`Workspace::lookup` / `read_file`, `crates/workspace/src/lib.rs`):

1. **conflict** — a structured conflict record wins (modeled; conflict storage
   is part of the source axis).
2. **overlay entry** — locally written `File`/`Symlink` bytes.
3. **tombstone** — an overlay `Tombstone` means the path is *absent* (the lookup
   returns "not found"), even if it exists in the base tree.
4. **base-ref (rename)** — an `OverlayKind::BaseRef`, the result of a clean
   rename / mode change: the path resolves to an existing Git blob with no bytes
   stored locally.
5. **base committed tree** — resolved by walking the base commit's tree
   component by component (`resolve_base_entry`), reading only the trees on the
   path.
6. **missing** — none of the above; the path does not exist in the working tree.

A directory can also exist *implicitly* because the overlay has descendants
beneath it (`overlay_has_descendant`), even with no base tree entry.

Reads never mark a path modified. `read_file` for a base or base-ref blob routes
through the object provider (which coalesces concurrent fetches and enforces the
fetch policy) and applies Git's working-tree filters with
`--attr-source=<base-commit>`; a symlink blob is the raw link target and is never
filtered.

## The three Git-facing trees

Status is a **three-tree** comparison (spec §11). The trees are:

* **HEAD / base** — the committed base tree of the current base commit
  (`base_commit`). Looked up lazily via `resolve_base_entry`.
* **staged** — the persistent staged delta in `glm-stage`
  (`crates/stage/src/lib.rs`): a `BTreeMap<RepoPath, StagedChange>` stored as a
  delta against HEAD (changed paths only), so it is `O(staged paths)`. A
  `StagedChange` is `Set { oid, mode }`, `Remove`, or `IntentToAdd`. The stage
  is a *third tree* distinct from both HEAD and the writable overlay; `add`
  records a staged blob here without touching the overlay, and `commit`
  materializes the staged delta onto HEAD.
* **working** — the overlay (`glm-overlay`, `crates/overlay/src/lib.rs`) layered
  over the base tree per the resolution order above.

## The XY status model

`Workspace::status` (`crates/workspace/src/lib.rs`) mirrors Git's porcelain XY
codes (`crates/workspace/src/status.rs`):

* **X = staged vs HEAD** — `code(head, staged)`.
* **Y = working vs staged** — `code(staged, work)`.

Each side yields a `StatusCode`: `Unmodified` (`.`), `Modified` (`M`), `Added`
(`A`), `Deleted` (`D`), or `TypeChanged` (`T`). A type change is detected by
comparing mode *classes* (regular/exec are one class; symlink, tree, and gitlink
are distinct), so a mode-only flip (exec bit) reports `Modified`, not
`TypeChanged`.

The comparison is over `(oid, mode)` pairs:

* HEAD ref = `base_entry_ref(path)` (the base tree entry, or `None`).
* Staged ref = the `StagedChange` resolved to `(oid, mode)`; `Remove` → `None`;
  `IntentToAdd` and "no staged change" fall through to the HEAD value.
* Working ref = `work_ref(path)`: a tombstone → `None`; a base-ref → its stored
  `(oid, mode)`; otherwise the overlay bytes are hashed.

### Why status is cheap and side-effect-free

* **`O(staged + overlay)`** — the candidate set is exactly the union of staged
  paths and overlay entries; the full tree is never enumerated (spec §49).
* **Never fetches blobs** — only `(oid, mode)` pairs are compared. The base side
  resolves tree metadata along the path; resolving HEAD `(oid, mode)` does not
  require the blob bytes.
* **Never writes objects** — working-tree oids are computed with a
  `git hash-object` **dry run** (`hash_blob_clean(..., write=false)` /
  `hash_blob_raw(..., write=false)`). The blob is hashed but not added to the
  object store, so `status` persists no dirty content (spec §2.7). (`add`, by
  contrast, calls the same hashing with `write=true` to stage the blob.)

The result is a list of `StatusEntry { path, index (X), worktree (Y) }`, filtered
to entries that actually changed.

## How this maps to the orthogonal model

The XY codes are a *projection* of the underlying axes for the user, not a
replacement for them. Status compares the three trees and reports difference; it
deliberately says nothing about residency. A path reported `.M` (clean index,
modified worktree) may have its content fully in the overlay or — for a base-ref
rename — no content cached at all. That separation is the whole point of the
four-axis model.
