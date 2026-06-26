# Index & scalability

How git-lazy-mount builds and presents a Git index without a full checkout, and
why that stays cheap on large repos. Companion overview:
[`architecture.md`](./architecture.md); spec: [`design.md`](./design.md). The
zero-blob-first-status design is owned by [`fsmonitor.md`](./fsmonitor.md); this
doc owns the index build and the interop bridge's synthesized index.

---

## 0. The scale question

Stock Git keeps one authoritative stage: `$GIT_DIR/index`. For a repo of `N`
tracked paths that index is `O(N)` entries on disk, and the cost of parsing it
bounds every `status`, `diff`, `add`, and ref-moving command. Three independent
costs must each be bounded:

| Cost | Driver | Where it bites |
|------|--------|----------------|
| **Index construction** | `O(N)` entries written at mount | one-time, at mount |
| **Index parse** | `O(entries)` per Git invocation | every `status`/`add`/`commit` |
| **Worktree scan** | `O(N)` `lstat`s unless suppressed | every `status` without FSMonitor |
| **Checkout eagerness** | `O(changed paths)` blob fetch + FUSE write | every `switch`/`reset --hard`/`merge` |

git-lazy-mount ships a single fixed strategy: a normal full index built without
fetching blobs, with the worktree scan suppressed by a pre-seeded FSMonitor
extension. The non-negotiable invariant: a `tree:0` mount fetches **zero**
working-file blobs to project the tree, and the first clean `status` (like every
subsequent one) fetches zero blobs and runs zero smudge filters.

---

## 1. The real index build

The index is built by `AdminRepo::build_index()`
(`crates/git-repo/src/lib.rs:191`), which runs exactly:

```
git read-tree HEAD
```

This populates `N` stage-0 entries from the HEAD tree. Under the default
`--filter=tree:0` clone (`crates/git-repo/src/lib.rs:50`) the HEAD tree hierarchy
is absent, so `read-tree` faults it — a one-time, bounded cost: the trees of
HEAD, not of all history. It fetches **zero blobs**; blobs hydrate only when a
path is read. The result is the single stage that stock `git add`/`status`/
`commit` operate on. There is no JSON staged delta and no second index.

> `tree:0` is the signature default, not `blob:none` or `--depth 1`. `blob:none`
> would download every tree from all of history; `--depth 1` would graft the
> commits and break `git merge`/`git rebase`. `tree:0` keeps history, merge-base,
> and branch switching working while still fetching no trees or blobs up front.
> See [`object-fetching.md`](./object-fetching.md) for the fetch path.

### 1.1 First clean status is zero-blob (FSMonitor pre-seed)

A freshly `read-tree`'d index has no FSMonitor extension, so Git's "mark all
valid" pass never runs and `status` would `lstat` (and so fault) every entry. The
mount avoids this by pre-seeding the extension at mount time:
`AdminRepo::seed_fsmonitor_valid()` (`crates/git-repo/src/lib.rs:238`) marks every
entry `CE_FSMONITOR_VALID` with the seq-0 token. Git's `refresh_cache_ent` then
early-returns on the valid bit *before* any `lstat`, so the first `status` faults
zero blobs, and the `git-lazy-mount-fsmonitor` hook answers "nothing changed" at
the seq-0 token.

The seed is **skipped wholesale** if any tracked `.gitattributes` declares a
checkout-conversion attribute (`filter=` / `ident` / `working-tree-encoding=` /
CRLF `eol=crlf`), since those paths' working-tree bytes diverge from the baseline
blob; an attribute read bounded by `SEED_ATTR_READ_TIMEOUT_SECS` (20s) makes that
decision. This zero-blob-first-status design is owned by
[`fsmonitor.md`](./fsmonitor.md); the token form, full-invalidation rules, and the
durable change journal (`crates/worktree/src/journal.rs`) live there.

`ls -l` / `stat` of an unmaterialized path is separate: it faults that path's blob
once for the exact size (trees carry no sizes). That is a `getattr` cost, not a
`status` cost.

### 1.2 What the mount actually configures

Mount configures only what is listed below (`crates/cli/src/main.rs:206`):

- `core.fsmonitor=<dir-of-exe>/git-lazy-mount-fsmonitor`
- `core.fsmonitorHookVersion=2`

It then seeds an empty journal and the FSMonitor valid bits (`seed_first_status`,
`crates/cli/src/main.rs:215`). No `index.version=4`, `feature.manyFiles`,
`core.untrackedCache`, or split-index config is set anywhere on the mount path.

---

## 2. The interop bridge's synthesized index

`crates/git-store/src/interop.rs` is a live, tested code path (exercised by
`store_integration.rs`), distinct from the mount: it lets stock `git` run against
the shared lazy store without a kernel mount. It stands up a throwaway
*operational* gitdir whose object I/O is routed into the shared store via
`GIT_OBJECT_DIRECTORY`, pins a detached HEAD (or a same-named branch) at the
workspace base, and synthesizes an index from the staged tree with
`git read-tree <tree>` (`interop.rs:102`).

It then marks **every** index entry skip-worktree via `mark_skip_worktree`
(`interop.rs:104`, `interop.rs:172`–`191`, `git update-index -z --skip-worktree
--stdin`). This is the universal-skip-worktree approach: with an empty/virtual
worktree, the skip bit on every entry stops Git from manufacturing spurious
deletions, so `git status` reflects exactly the staged delta and `git commit`
records the synthesized index verbatim. New objects (including the commit object)
land directly in the shared store, and the caller reads back the bridge HEAD to
adopt the commit into the workspace.

The synthesized index is disposable: the stale index file is removed and rebuilt
on each call. The bridge never hand-encodes the index binary format — it drives
stock `read-tree` and `update-index` plumbing.

---

## 3. Checkout / switch / rebase eagerness

Branch-changing commands (`switch`, `checkout`, `reset --hard`, `merge`,
`rebase`) are correct but **potentially eager**: stock Git writes every changed
path through the FUSE write path. This is bounded by the size of the tree delta,
`O(M)`, not by the repo size `O(N)` — a switch over an M-of-N delta touches `O(M)`
blobs. Clean, untouched paths are not re-fetched or re-written.
