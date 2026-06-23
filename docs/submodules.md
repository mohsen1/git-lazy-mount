# Submodules

*(spec §33)*

> **Status — honest summary.** Submodule support is **design**, with one piece
> modeled and exercised today: the **gitlink mode** (`GitMode::Gitlink`, Git
> mode `160000`) is parsed, round-tripped, and projected as a directory entry.
> Everything below describing `.gitmodules` handling, modes, URL validation,
> dirty tracking, cycle detection, and nested mounts is the **intended design**
> and is **not implemented yet**. In particular, **nested lazy-mount creation
> is not implemented.**

## What exists today

`glm-core::GitMode::Gitlink` models a submodule commit pointer
([crates/core/src/mode.rs](../crates/core/src/mode.rs)):

* it parses from / serializes to the canonical tree mode `160000`;
* in the state model it is a distinct `Source`/`EntryKind` (`Gitlink`), not a
  file or a tree (see [state-model.md](state-model.md) and
  [architecture.md](architecture.md));
* it **projects as a directory entry** — `to_unix_mode()` reports `0o040755`
  and the filesystem layer maps it to a directory-like mount point — so a
  submodule appears as a directory in the tree, with the gitlink's target
  commit oid recorded as the entry's object id.

That is the full extent of current submodule behavior: the gitlink is *modeled
and visible*, not *populated*.

## Intended design

### Detection

Detect a submodule by its tree entry mode: `160000` (`GitMode::Gitlink`). The
entry's object id is the **commit** the superproject pins, not a tree or blob.

### Modes

A configurable submodule policy:

| Mode | Behavior |
| --- | --- |
| `none` | Submodules are visible as gitlink directory entries; their content is never populated. |
| `lazy` | A submodule may be mounted on demand (a nested lazy-mount), populated only when entered. |
| `recursive` | Submodules are set up as part of the parent, transitively. |

`lazy`/`recursive` imply creating nested workspaces — see *Honesty* below.

### Respect `.gitmodules`

Read submodule definitions (path → name → URL) from the superproject's
`.gitmodules`. `.gitmodules` is **repository-controlled, untrusted data** (see
[security.md](security.md)): it is parsed as configuration, never executed, and
its declarations are validated before use.

### Validate submodule URLs

Submodule URLs and paths must be **validated against URL and path injection**
before any fetch or directory creation: reject URLs that smuggle options or
shell-like content, and reject submodule paths that escape the superproject
(the same `RepoPath` rules — no traversal, no absolute, no empty component;
see [crates/core/src/path.rs](../crates/core/src/path.rs)). As everywhere in
git-lazy-mount, `git` is invoked via `argv`, never a shell.

### Dirty state tracked separately

A submodule's own modifications are tracked as **its own** dirty state, distinct
from the superproject's. This mirrors the orthogonal state model: the
superproject sees the submodule as one gitlink entry whose recorded commit may
differ from the submodule's current head; the submodule's internal file changes
belong to the submodule's workspace, not the parent's.

### Commit only the gitlink oid

When committing the superproject, git-lazy-mount records **only the gitlink
commit oid** for the submodule entry — exactly as Git does. The parent commit
references the submodule by commit id; it never inlines the submodule's tree.

### Detect cyclic configuration

Submodule configuration can form cycles (A includes B includes A, or a
self-reference). Cyclic `.gitmodules` graphs must be **detected and refused**
rather than followed into unbounded recursion.

### Never fetch from a directory listing

Listing a directory that contains a submodule must **never** trigger an
automatic network fetch of that submodule. Enumeration is metadata-only
(consistent with `readdir` being `O(entries)` and fetch-free; see
[performance.md](performance.md)). Populating a submodule is an explicit action.

### Networked submodule init requires trust

Initializing a submodule that reaches the **network** requires that the
repository be **trusted** (`git lazy-mount trust grant`; see
[security.md](security.md)). Without trust, networked submodule initialization
does not run — the same gate that governs external filters and hooks.

## Honesty

* The gitlink mode is **modeled and projected as a directory entry**; that part
  is real and tested.
* `.gitmodules` parsing, the `none`/`lazy`/`recursive` modes, URL/path
  injection validation, separate dirty-state tracking, gitlink-only commit
  semantics, cycle detection, the no-fetch-on-listing guarantee, and the
  trust-gated networked init are all **design** and **not implemented yet**.
* **Nested lazy-mount creation is not implemented.** `lazy`/`recursive` cannot
  populate a submodule today because there is no mechanism to spin up a nested
  workspace for it.
