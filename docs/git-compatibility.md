# Git compatibility

Native `git lazy-mount` commands are the **authoritative** interface. Ordinary
`git` commands are divided into explicit tiers; we do **not** advertise a
command as supported until an integration test (with a hydration-budget
assertion) proves it.

## Compatibility tiers (spec §37)

* **Level A — native lazy-mount commands.** These operate directly on the
  transactional workspace model and are the supported source of truth.
* **Level B — read-only Git access.** Safe queries (`log`, `show`, `rev-parse`,
  `cat-file`, `branch --list`) run against the backing store; each must be
  proven genuinely read-only.
* **Level C — wrapped mutating Git commands.** The wrapper begins a transaction,
  exports a temporary index, invokes Git with controlled `GIT_DIR`/
  `GIT_INDEX_FILE`/`GIT_WORK_TREE`, then imports the result. Every such command
  requires an integration test and a hydration-budget assertion.
* **Level D — arbitrary direct Git.** A raw `.git` facade is **not** exposed by
  default; an opt-in mode may expose one only after proving it cannot bypass
  overlay durability, invalidate the operation log, hydrate the whole repo, or
  corrupt another workspace.

## Current matrix

This matrix reflects what is **implemented and tested today**, not aspirations.
"Native" = implemented as a `git lazy-mount` subcommand against the workspace
engine and covered by tests.

| Command            | Tier | Status (today) | Notes |
|--------------------|------|----------------|-------|
| `status`           | A | **native, tested** | three-tree XY, O(changed), no fetch/scan |
| `diff` (names)     | A | native (name-status) | content diff is future |
| `add` / `unstage`  | A | **native, tested** | clean filters via Git plumbing |
| `restore`          | A | native | worktree restore from base |
| `commit`           | A | **native, tested** | ordinary Git commit; subtrees reused |
| `push`             | A | **native, tested** | `--force-with-lease` CAS to bare remote |
| `fetch`            | A | partial | store-level fetch implemented |
| `branch` lease/CAS | A | **native, tested** | attached-branch compare-and-swap |
| `switch`           | A | partial | clean switch designed; dirty policies future |
| `reset`            | A | future | soft/mixed/hard semantics designed (§36) |
| `merge`/`rebase`   | A | future | structured-conflict model designed (§34) |
| `clean`/`stash`    | A | future | — |
| `log`/`show`/`rev-parse`/`cat-file` | B | available via backing store | prove read-only before advertising |
| wrapped mutating   | C | future | requires the temp-index wrapper + tests |
| arbitrary `.git`   | D | not exposed | by design |

`git lazy-mount op log` exposes the operation history; `git lazy-mount git -- …`
(Level B/C wrapper) is the planned entry point for stock Git.

## Finding: tree entry modes must be `40000`, not `040000`

When constructing tree objects we hash the canonical raw tree byte stream
(`git hash-object -t tree`). Git serializes a **subtree** entry's mode as
`40000` (five digits, no leading zero); emitting the zero-padded `040000`
produces a *"zero-padded file mode"* that `git fsck` rejects:

```
error: object … fails fsck: zeroPaddedFilemode
fatal: refusing to create malformed object
```

`glm-core::GitMode::as_octal()` therefore returns `40000` for trees, and
`commit_reuses_subtrees_and_passes_fsck` asserts that a commit whose tree
contains a subdirectory passes `git fsck --connectivity-only`. Differential
testing against real Git caught this.

## Interoperability guarantees we DO make

* Commits are ordinary Git commits a normal bare server accepts (verified: the
  CLI e2e test pushes to a bare remote and reads the file back from it).
* Tree objects we write are byte-identical to what `git write-tree` would
  produce for the same entries (canonical sort + exact mode serialization).
* Object ids are format-agnostic (sha1/sha256/other); we never assume 40 hex
  chars.
* Working-tree filtering is performed by Git's own plumbing, so projected clean
  bytes match a real checkout under the same effective configuration (see
  [filters-and-lfs.md](filters-and-lfs.md)).

## What we explicitly do NOT claim

Transparent compatibility with arbitrary porcelain, transparent lazy
commit-graph history, LFS locking, and a `.git` facade are **not** provided.
See [limitations.md](limitations.md). A generated compatibility matrix from the
test suite supersedes any optimistic manual claim.
