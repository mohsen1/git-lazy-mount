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
| `log`/`show`/`rev-parse`/`cat-file`/`diff` | B | **implemented, tested** | stock git via `git lazy-mount git …`; lazy-fetches through the bridge |
| `status` (stock git) | B | **implemented, tested** | reads "On branch …"; reflects the staged tree (see below) |
| `commit` (stock git) | C | **implemented, tested** | synthesized index → stock `git commit` → adopted as the new base |
| other mutating     | C | not exposed (default-deny) | use the native `git lazy-mount` equivalent |
| arbitrary `.git`   | D | not exposed | by design |

`git lazy-mount op log` exposes the operation history; `git lazy-mount git -- …`
is the implemented entry point for stock Git (Levels B and C; see below).

## The `git lazy-mount git -- …` bridge

`git lazy-mount git -- <args>` runs **stock `git`** against the shared lazy
store so native commands work without a kernel mount. It is validated
end-to-end against real `git` (`crates/git-store/tests/store_integration.rs ::
interop_bridge_status_commit_and_lazy_fetch` and `crates/cli/tests/cli_e2e.rs ::
git_interop_bridge_status_and_native_commit`).

### How it works

The bridge stands up a throwaway *operational gitdir* and, on each invocation:

1. **Routes object I/O into the shared store** via `GIT_OBJECT_DIRECTORY=<store>/
   objects`. Reads see every base object; new objects (notably the commit
   created by `git commit`) land directly in the store.
2. **Pins HEAD** to the workspace base. When a branch is attached, HEAD points
   at a same-named branch in the throwaway repo, so output reads `On branch
   main` rather than a detached-HEAD note.
3. **Synthesizes the index** from the workspace *staged tree* (the base tree
   with the staged delta folded in) with `read-tree`, then marks **every entry
   `skip-worktree`**. This is the same mechanism sparse-checkout uses: an empty
   (virtual) worktree no longer manufactures spurious "deleted" entries, so
   `git status` shows exactly the staged delta and **nothing is materialized**.
4. **Mirrors the store's promisor remote** into the operational gitdir, so a
   blob absent from the store is still lazy-fetched on demand (e.g. by `git show
   HEAD:path`) and lands in the shared store. (Verified: glm's store lazy-fetches
   via `remote.origin.promisor=true` even though it does not set
   `extensions.partialClone`.)
5. **Inherits stdio**, so the user's editor (`git commit` with no `-m`) and
   pager (`git log`) behave natively.

After the run the bridge reads back HEAD. For `commit`, the new commit is
**adopted** into the workspace (`Workspace::adopt_commit`): the private head ref
and attached branch advance with compare-and-swap, the stage is cleared,
now-clean overlay entries are dematerialized, and the transaction is sealed in
the operation log — identical bookkeeping to a native `git lazy-mount commit`.
The adopted commit's first parent **must** be the current base, so history
rewrites (`--amend`, rebase) are rejected, not silently mis-recorded.

### Safety: default-deny

Only an explicit read-only allowlist (`status`, `log`, `show`, `diff`,
`cat-file`, `rev-parse`, `ls-files`, `ls-tree`, `blame`, …) and `commit` are
permitted. Everything else is refused with a pointer to the native command.
This is a **default-deny** posture: destructive object maintenance (`gc`,
`prune`, `repack`) can never reach the shared store through the bridge, and
`git commit -a`/`--patch`/`--amend` are rejected because the working tree is
virtual. Staging is always done with `git lazy-mount add`.

### Limitations without a kernel mount

Through the bridge there is no populated worktree, so stock `git status` reports
the **staged** view ("Changes to be committed") but cannot show *unstaged*
working-tree edits — those live in the overlay and are visible via
`git lazy-mount status` (the full three-tree view). A fully transparent,
lazily-projected `git status`/`git checkout` against a real `.git` requires the
kernel filesystem backend (FUSE/FSKit/ProjFS); that is the GVFS/Scalar model and
is tracked separately (Level D is still not exposed).

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
