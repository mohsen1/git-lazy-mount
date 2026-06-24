# Virtual working-tree model: baseline + overlay

This area of the [specification](design.md) covers, primarily **§8** (the
custom state represents *only* the virtual working tree), **§29** (rename
semantics), **§30** (symlinks / hard links / special files), **§31** (raw
repository paths). Cross-references: §7 (Git is authoritative), §25 (stock-Git
index behavior), §14–§17 (inode/handle model), §23 (filters/attributes), §32
(overlay durability). This is a design doc for the *design*: the custom stage,
custom branch DB, commit-adoption, and `git lazy-mount git --` bridge are
**superseded** and out of scope here (§4.2, §4.3, §1).

This document specifies what bytes the working tree contains, independent of
`HEAD` and the index. It does **not** specify staging, commit, refs, or status —
those are Git's, served from the real `$GIT_DIR/index` and refs (§7, §25).

---

## 1. Scope and the two-source-of-truth boundary

| Owner | State this doc governs? |
|-------|-------------------------|
| **Git** (native admin gitdir) | No — `HEAD`, refs, reflogs, the real index + conflict stages, commit/merge/rebase state (§7). |
| **Daemon** (this doc) | Yes — the *working-tree bytes*: baseline + overlay + tombstones + synthetic entries; inode/namespace identity; rename mappings. |

The working tree is a **pure projection** computed from four daemon-owned inputs
plus Git objects. It answers exactly one question (§8):

> What bytes would this path contain in the logical working tree, right now?

It must never answer "what is staged", "what is HEAD", or "what branch is
checked out". Those flow from Git. The daemon's parses of Git state are
disposable caches (§7); the working-tree model is *not* — it holds acknowledged
user bytes and must be durable (§32, §8.2).

### Reuse map (existing code)

| Concept | Existing file | Design disposition |
|---------|---------------|----------------------|
| `RepoPath` (raw bytes) | `crates/core/src/path.rs` | **Keep as-is.** Already meets §31. |
| `Overlay` + `OverlayKind` (`File`/`Symlink`/`BaseRef`/`Tombstone`) | `crates/overlay/src/lib.rs` | **Keep**; add `Synthetic` resolution above it; drop dependence on the custom stage. |
| `InodeTable` (stable identity, open-unlink, rename) | `crates/fs-common/src/inode.rs` | **Keep.** Already meets §14. |
| `GitMode`, `TreeEntry`, `TreeObject` | `crates/core/src/{mode,tree}.rs` | **Keep.** |
| `EntryKind`, baseline resolution, `list_dir`, CoW write primitives | `crates/workspace/src/lib.rs` | **Salvage the resolution + write logic; discard** the `Stage`, `attached_branch`/`workspace_head_ref`, `commit`/`adopt_commit`/`merge`/`switch`/`reset` (superseded by stock Git). |
| `Source`/`SemanticStatus`/`Residency` axes | `crates/core/src/state.rs` | **Keep `Residency`/`Source`** for diagnostics; `SemanticStatus` is now Git's to report, not ours. |

The salvageable resolution lives today in `Workspace::lookup` /
`Workspace::list_dir` / `Workspace::read_file` (`crates/workspace/src/lib.rs`
lines ~278–542). The design extracts it into a `worktree` crate (§41) with no
stage, no refs, no oplog-as-history.

---

## 2. The model: baseline + overlay + tombstones + synthetic entries

```
working_tree(path) = resolve(synthetic, overlay, baseline)
```

Four inputs:

1. **Synthetic entries** — daemon-reserved control paths. Exactly one today: the
   root `.git` gitfile (§6). Read-only, fixed inode, protected from
   unlink/rename/replace/chmod/write/mkdir-beneath.
2. **Overlay** — durable native files + a transactional namespace DB holding
   *locally materialized* state: written files, created symlinks, **base-refs**
   (a clean rename pointing at an existing blob with no bytes copied), and
   **tombstones** (deletions). `crates/overlay/src/lib.rs`.
3. **Baseline** — a committed Git **tree** oid: "what an unmaterialized path
   would contain". Lazy; nothing is fetched to hold a baseline (§38.1).
4. **Absent** — neither synthetic, overlay, nor baseline resolves the path.

Initial state at mount (§8, §10.3): `baseline = HEAD-commit tree`,
`overlay = empty`.

```rust
/// The daemon's complete working-tree definition (one per mount).
pub struct Worktree {
    /// Committed tree the unmaterialized projection reads from. NOT HEAD,
    /// NOT the index — see §3. Guarded so advancement is atomic (§4).
    baseline: RwLock<Baseline>,
    overlay: Overlay,            // crates/overlay
    inodes: InodeTable,          // crates/fs-common
    provider: Arc<dyn ObjectProvider>, // streaming Git objects (§20)
    synthetic: SyntheticTable,   // reserved control entries (§6)
}

/// A baseline is a tree oid plus the commit it came from (the attribute
/// source for filters; §23) and a monotonic generation (§14, §22).
pub struct Baseline {
    commit: Option<ObjectId>, // None only for an unborn HEAD
    tree: Option<ObjectId>,   // commit^{tree}
    generation: u64,          // bumps on every advancement (§4, §22.dir-mtime)
}
```

`Baseline` carries the **commit** (not just the tree) because filter/attribute
resolution needs an `--attr-source` tree-ish (§23; `GitStore::smudge_blob`'s
`attr_source` parameter, `crates/git-store/src/store.rs`).

---

## 3. Path resolution order (the 6 steps)

Verbatim from §8, made precise. `resolve(path)` returns a `Resolved`:

```rust
pub enum Resolved {
    Synthetic(SyntheticEntry),                 // step 1
    Overlay(OverlayKind),                      // steps 2–4 (one DB lookup)
    Baseline { oid: ObjectId, mode: GitMode }, // step 5
    ImpliedDir,                                // dir implied by an overlay descendant
    Absent,                                    // step 6
}
```

| # | Source | Condition | Result |
|---|--------|-----------|--------|
| 1 | **Synthetic** | `synthetic.get(path).is_some()` | `Synthetic(_)`. Wins over everything; a baseline/overlay collision with a reserved path is rejected (§6, see §8 below). |
| 2 | **Overlay content** | `overlay.entry(path)` ∈ {`File`, `Symlink`} | `Overlay(kind)`. Local bytes (or link target) from the overlay store. |
| 3 | **Overlay tombstone** | `overlay.entry(path) == Tombstone` | `Absent` (the deletion shadows the baseline). |
| 4 | **Overlay rename / base-ref** | `overlay.entry(path) == BaseRef{oid,mode}` | `Baseline{oid,mode}`-equivalent: content streams from `oid`, *no fetch to relocate* (§29). |
| 5 | **Baseline tree** | `resolve_base_entry(path)` is `Some(entry)` | `Baseline{entry.oid, entry.mode}` (walks trees component-by-component; one tree object per level — §15, §38.2). |
| 6 | **Absent** (with implied-dir check) | none of the above; but if any non-tombstone overlay entry has `path` as a strict prefix → `ImpliedDir` | else `Absent`. |

Notes:

- Steps 2–4 are **one** namespace-DB read (`overlay.entry`); the table dispatches
  on `OverlayKind`. This is already how `Workspace::lookup` is structured.
- Step 5's walk must use a **cache-only** fetch policy for *passive* reads (a
  tree object missing locally is faulted only by the dedicated fetch scheduler,
  never inline under an inode lock — §19). Tree objects for the committed
  baseline are present after a `blob:none` clone, so step 5 normally hits cache.
- **Implied directories** (step 6) exist because the overlay stores leaves; a
  written `a/b/c` with no entry for `a/b` still makes `a/b` a directory. This is
  `Workspace::overlay_has_descendant`. Empty *untracked* directories that are
  not implied by any descendant are persisted as explicit namespace records so
  they survive remount (§4.9, §15).

`readdir(dir)` is the union of (a) the baseline tree at `dir` and (b) overlay
entries whose parent is `dir`, with tombstones subtracting and base-refs/files
adding — O(direct children), never O(dirty paths) (§15, §38.2). It returns
names + `d_type` only; **never** sizes or blob reads (§4.5, §21). This is
`Workspace::list_dir`.

---

## 4. Why a baseline is necessary (§8.1)

An index-only Git operation changes `$GIT_DIR/index` **without** touching
working-tree bytes:

```bash
git reset --mixed <commit>      # index ← <commit> tree; worktree unchanged
git restore --staged <path>     # index entry ← HEAD; worktree unchanged
git rm --cached <path>          # index entry removed; worktree FILE STAYS
git update-index --cacheinfo …  # index entry forced; worktree unchanged
```

If the projection sourced "unmaterialized" content from the **index**, every one
of these would silently rewrite or delete working-tree files — a correctness
disaster and a direct violation of §25.1 / release-criterion 19–20 (§43).

The baseline decouples the two: it is the *content source for unmaterialized
clean paths*, advanced **only** when Git actually wrote the working tree (§4),
while the index moves independently under Git's control. Symmetrically, a
working-tree edit goes to the **overlay**, never the index — the index changes
only when the user runs `git add` (§25.2), which Git does through the real index.

**Testable invariant (W-BASELINE-1):** after `git rm --cached p` and
`git reset --mixed`, `cat p` returns the same bytes as before, and `readdir`
still lists `p`. Differential against a conventional checkout (§40.1).

---

## 5. Baseline advancement (§8.2)

The baseline advances **only after a known worktree-updating command**, detected
out-of-band — never inferred from the index and **never** from a timestamp
(§8.2, §25.2: "Do not infer worktree updates solely from a changed index").

### 5.1 Triggers

Detection sources, in trust order:

1. **Provider notification hooks** (§13): `post-checkout`, `post-merge`,
   `post-commit`, `post-rewrite`, `post-applypatch`. These fire *because the user
   ran a Git command that updates the worktree*. The hook is a thin IPC client
   (§12.4, §13.1) that hands the daemon `(old_head, new_head, flag)`.
2. **Reconcile-from-disk** (§13, §32.2) on daemon restart or a missed event:
   read `HEAD`, compare to the recorded baseline commit, and if they differ,
   treat it as an advancement to the new `HEAD`.

A *bare* index mutation (no worktree write) produces `post-index-change` but
**no** `post-checkout`/`post-merge` — so it does **not** advance the baseline.
That is the mechanism that makes §4 true.

### 5.2 The advancement transaction

```rust
/// Advance the baseline to `new_commit` after a confirmed worktree update.
/// Atomic w.r.t. resolution: readers see the old or the new baseline, never a
/// torn pair. Overlay entries are PRESERVED (see 5.3).
fn advance_baseline(&self, new_commit: ObjectId) -> Result<u64>;
```

Steps:

1. Resolve `tree = new_commit^{tree}` (cache-only; the checkout already faulted
   what it wrote).
2. Take the `baseline` write lock; set `{commit, tree}`; **bump `generation`**.
3. Bump the inode generation for *newly allocated* inodes only
   (`InodeTable::bump_generation`); existing open handles keep their generation
   so they are unaffected (§14, `crates/fs-common/src/inode.rs`).
4. Invalidate projection caches for paths whose baseline entry changed
   (directory mtimes for changed dirs — §22, §12.3).
5. **Do not clear the overlay.** Local modifications, tombstones, and base-refs
   survive a baseline change (§8.2) so a concurrent edit is never lost.

The generation bump is what lets FSMonitor detect a discontinuity and what keeps
synthetic directory mtimes changing when children change (§22, §12.1).

### 5.3 Why overlay entries survive

Consider an unmaterialized file `p` that the user edits (overlay `File`), then
`git stash` writes the worktree back to clean: the *checkout* materializes the
clean bytes by writing through FUSE, which lands in the overlay as the new clean
content, and `advance_baseline` moves to the new tree. The overlay entry is
preserved across the advance; only **compaction** (5.4) may later drop it, and
only after proving equality. Eagerly clearing the overlay on advancement would
discard an edit that races the checkout — forbidden (§8.2, §32.2 step 3
"preserve every file containing acknowledged user writes").

### 5.4 Dematerialization (compaction) preconditions

Compaction is a *separate, later, optional* pass (§8.2: "Initially favor
correctness over aggressive compaction"). An overlay entry for `p` may be
dropped (reverting `p` to lazy baseline resolution) **only when all** hold:

```rust
fn may_dematerialize(&self, p: &RepoPath, fh: &HandleTable) -> bool {
    let base = self.resolve_base_entry(p, CacheOnly).ok().flatten();
    matches!(base, Some(b) if
        content_equals_baseline(p, &b)   // 1. bytes == projected baseline bytes
        && mode_equals(p, &b)            // 2. Git-relevant mode matches (exec/symlink)
    )
    && !fh.has_writable_open(p)          // 3. no writable handle open
    && !fh.has_pending_fsync(p)          // 4. no pending fsync
    && !self.rename_refs(p)              // 5. no in-flight rename references it
    // 6. NEVER by timestamp (§8.2) — time is not an input here.
}
```

Conditions 1–5 are §8.2 verbatim; condition 6 is the prohibition. "Content
equals baseline" compares the *filtered working-tree representation* of the
baseline blob (CRLF/encoding/ident/LFS-aware, §21, §23), not the raw blob —
otherwise a CRLF repo would never compact. Dematerialization calls
`Overlay::clear` (`crates/overlay/src/lib.rs`).

**Testable invariants:**
- **W-ADV-1:** index-only commands never advance the baseline (no
  `post-checkout`/`post-merge` ⇒ no advance).
- **W-ADV-2:** advancement preserves a concurrently-written overlay entry
  (crash/race test, §40.5).
- **W-ADV-3:** compaction never runs with a writable handle open or a pending
  fsync (model test, §40.4).
- **W-ADV-4:** no code path keys dematerialization on mtime/ctime (grep-level +
  property test that a stale timestamp alone never drops an entry).

---

## 6. RepoPath: raw bytes as identity (§31)

Already implemented in `crates/core/src/path.rs` and meeting §31. Restated as the
contract the worktree model depends on:

```rust
pub struct RepoPath { bytes: Vec<u8> } // identity = raw bytes, never lossy UTF-8
```

| Requirement (§31) | Mechanism |
|-------------------|-----------|
| No lossy UTF-8 for identity | `as_bytes()` is the only identity; `Hash`/`Eq`/`Ord` are over bytes. `display()` is lossy and explicitly *not* parseable back. |
| NUL-delimited plumbing | Git invocations use `-z`/NUL framing; `RepoPath` forbids embedded NUL so a path can never inject a delimiter. |
| Safe display / JSON escaping | `escape()`/`unescape()` round-trip percent-encoding; `serde` uses `escape()`, so non-UTF-8 paths survive JSON (the namespace DB and IPC). |
| No shell construction; no `rev:path` | Object access is by **oid**, never `rev:path`. The one path-taking plumbing call (`cat-file --filters --path=`) requires UTF-8 and is gated — see below. |
| Attribute lookup not stopped at first non-UTF-8 component | Resolution walks `components()` (byte slices); it never UTF-8-decodes to traverse. |

**Rejected on construction** (`from_bytes`, already enforced & unit-tested):
embedded `NUL`, absolute (`/`-leading), `.`/`..` traversal, empty components
(`a//b`). Reserved internal control paths (the synthetic `.git`, §6) are rejected
*at the namespace layer* (a baseline/overlay write to a reserved path fails
safely, §8 below), not in `from_bytes` (the bytes themselves are legal Git path
bytes).

### 6.1 The non-UTF-8 filter gap (must-fix in design)

`GitStore::smudge_blob` / `hash_blob_clean` (`crates/git-store/src/store.rs`)
pass `--path=<utf8>` to `git cat-file --filters` and **error** on non-UTF-8
paths. §31 forbids "stopping attribute lookup at the first non-UTF-8 component".
Resolution:

- For paths where `.gitattributes` selects **no** filter (the common case),
  serve the **raw blob** directly (no plumbing path argument needed) — correct
  and byte-exact.
- For paths needing a filter *and* containing non-UTF-8 bytes, drive the filter
  via a mechanism that accepts raw path bytes (a long-running
  `git filter-process` protocol-v2 session, which is NUL/length-framed, §23), not
  `--path=`. Until that exists, such a read returns a bounded, *escaped*-path
  error (§31 safe display) rather than a silent wrong answer.

**Testable invariant (W-PATH-1):** a tracked file whose path contains invalid
UTF-8, a newline, a tab, a leading dash, a backslash, and quotes is listed, read
(unfiltered), renamed, and tombstoned correctly; its JSON/log representation
round-trips (§40.7).

---

## 7. Rename semantics (§29)

A rename changes the *namespace*, not necessarily content. The model preserves
file identity (inode) and avoids blob fetches for clean renames.

### 7.1 Clean rename = metadata only

The key mechanism is `OverlayKind::BaseRef { oid, mode }`
(`crates/overlay/src/lib.rs`; ADR 0005): place a reference to the **existing
blob** at the new path. No bytes are copied or fetched (§29: "A clean file rename
should be representable as metadata referring to the same blob without fetching
its contents"; §38.9). The source gets a tombstone (if it was a baseline path) or
its overlay entry is moved.

```rust
fn rename(&self, from: &RepoPath, to: &RepoPath, f: RenameFlags) -> Result<()>;
```

Resolution of `from` (mirrors `Workspace::rename`):

| `from` resolves to | Action at `to` | Fetch? |
|--------------------|----------------|--------|
| Overlay `File`/`Symlink` | move content record to `to` | none |
| Overlay `BaseRef{oid,mode}` | `put_base_ref(to, oid, mode)` | none |
| Baseline file/symlink | `put_base_ref(to, oid, mode)` | none |
| Baseline **tree** (subtree) | see 7.2 | none for clean descendants |
| Tombstone / Absent | `ENOENT` | — |

Then `from` is deleted (tombstone if baseline-backed, else `clear`). The inode
moves with the content (`InodeTable::rename`), so open handles on `from` keep
working (§14, §29 "rename with open source and destination handles").

### 7.2 Clean subtree rename = no descendant reads (§29)

The current code returns *unsupported* for directory rename
(`crates/workspace/src/lib.rs` ~532). The design **must** implement it as a
metadata operation: re-parent the subtree in the namespace DB
(`rename subtree`, §15) so the baseline tree at `from/...` is logically relocated
to `to/...` **without reading any descendant blob** (§29: "A clean subtree rename
should not read descendant blobs"; §38.9).

Representation options (choose by measurement, §39):

- **(a) Subtree-mapping record** — a single namespace entry "`to` ⇒ baseline
  subtree `oid_of(from)` at generation `g`"; resolution of `to/x/y` walks the
  mapped baseline tree. O(1) to record, O(depth) per descendant lookup.
- **(b) Eager namespace re-parent** — rewrite parent pointers of *direct*
  children only, recursing lazily on access. O(direct children) up front.

Either way: **zero blob fetches**, descendant content still streams from the
original blobs. Mixed subtrees (some descendants already in the overlay) re-parent
the overlay entries *and* carry the baseline mapping for the clean remainder.

### 7.3 Filter-context invalidation (§29, §23)

"Changing a path may change its Git filter context; invalidate affected filtered
cache entries." A rename changes the path that `.gitattributes` matching keys on,
so the **filtered-content cache** key (which includes path bytes + attribute
state, §23) changes. On any rename:

- Invalidate the filtered-cache entry for `from` **and** for `to` (the new path
  may match a *different* attribute, e.g. moving into a `*.txt → CRLF` directory).
- A base-ref/subtree rename caches *nothing new*; it just drops stale `from`
  cache entries. The raw blob is unchanged and shared.

**Testable invariants:**
- **W-RENAME-1:** renaming an unmaterialized clean file fetches **0** blobs
  (hydration-budget assertion, §38.9).
- **W-RENAME-2:** renaming an unmaterialized clean **directory** of N files
  reads 0 descendant blobs.
- **W-RENAME-3:** renaming `a.bin` → `b.txt` across a `* -text` → `*.txt text`
  attribute boundary yields the CRLF-correct bytes for `b.txt` (filter context
  re-evaluated, not the stale `a.bin` result) (§40.8).
- **W-RENAME-4:** `RENAME_NOREPLACE` over an existing `to` fails with `EEXIST`;
  `RENAME_EXCHANGE` is either implemented or returns a documented error (§29);
  case-only rename works on a case-sensitive mount; rename with open
  source+dest handles keeps both handles valid (§14).

---

## 8. Synthetic entries and the protected `.git` (§6)

`SyntheticTable` holds daemon-reserved control entries. The only one in the MVP
is the root `.git` regular file (§6):

```rust
pub struct SyntheticEntry {
    pub ino: u64,            // reserved, stable (§14: "root .git gitfile reserved inode")
    pub kind: SyntheticKind, // GitFile { gitdir: PathBuf }
    pub readonly: bool,      // always true
}
```

- Content is exactly `gitdir: <abs path to admin gitdir>\n` (§6, §10.6 health
  check). Served from a fixed buffer; no overlay, no Git object.
- **Resolution priority 1** (§3): it shadows any baseline/overlay path of the
  same name. A Git **tree** that contains a literal `.git` entry at the root (a
  malicious or pathological repo, §36) must **fail safely** — the synthetic entry
  wins and the tree entry is suppressed/reported, never projected (§6: "A Git
  tree entry that conflicts with Git's protected `.git` namespace must fail
  safely").
- Protected operations on `.git`: `unlink`, `rename` (as source or dest),
  replace, `chmod`/`setattr`, `write`, and `mkdir`/`create` beneath it all
  return `EPERM`/`EACCES` (§6). The namespace layer rejects any overlay write
  whose path is, or is under, a reserved path.

**Testable invariant (W-SYNTH-1):** `cat <mnt>/.git` == `gitdir: <expected>`;
`rm`, `mv`, `chmod`, `echo x > .git`, and `mkdir .git/x` all fail; a repo whose
root tree contains a `.git` blob still mounts and that entry is not projected
(§40.7 "root .git collision attempts").

---

## 9. Symlinks, hard links, special files (§30)

### 9.1 Symlinks (§30.1)

On Linux, project Git symlinks (`GitMode::Symlink`, blob bytes = target) as
**native symlinks**. Overlay symlinks store the raw target bytes
(`Overlay::put_symlink`, `read_content`).

- Preserve **raw target bytes** exactly: broken targets, relative *and* absolute
  targets, non-UTF-8 targets. Symlink blobs are **never filtered**
  (`Workspace::read_blob_for_mode` uses `raw_blob` for `Symlink` — keep).
- **Never follow repository symlinks for internal overlay writes** (§30.1, §36
  symlink-race): overlay content/meta files are addressed by a hash of the path
  bytes (`Overlay::id_for`), so the daemon never `open`s a repo-controlled path
  for its own storage. This structurally avoids the symlink-swap TOCTOU class.
- Symlink-swap race protection (§36): `getattr`/`readlink`/`open` resolve through
  the inode (identity), and resolution is a single namespace read, so a path that
  flips file↔symlink between calls cannot trick a write into following a link.

### 9.2 Hard links (§30.2)

Git does not preserve hard-link identity. **MVP policy: return a clear
unsupported error** from `link()` (`EOPNOTSUPP`), documented in `limitations.md`.
The alternative (overlay hard links that lose identity at commit) is deferred and
must be *explicit* if ever adopted — never silently copy while pretending
identity was preserved (§30.2). `link` is already listed as "or a clearly
documented error" in §16.

### 9.3 Special files (§30.3)

Device nodes, sockets, FIFOs, and unsupported reparse-style objects are
**rejected** (`mknod` → `EPERM`) in the MVP. They cannot appear in a Git tree
(no such `GitMode`), so they could only arise via the overlay; refusing them
keeps the overlay representable as Git content. If ever supported they are
**overlay-only** and explicitly non-committable (§30.3, mirrors the
never-committed screening in `glm_platform::metadata`).

**Testable invariants:**
- **W-LINK-1:** a tracked broken symlink, a relative symlink, an absolute
  symlink, and a non-UTF-8-target symlink all `readlink` byte-exactly and are
  never hydrated/filtered (§40 differential).
- **W-LINK-2:** `link()` returns `EOPNOTSUPP`; `mknod` of a fifo/device returns
  `EPERM`; neither corrupts the namespace.
- **W-LINK-3:** flipping a path between regular file and symlink under a
  concurrent reader never causes a write to follow the link (§36, §40.6).

---

## 10. Invariant summary (regression-test targets)

| ID | Invariant | Spec | Source of truth |
|----|-----------|------|-----------------|
| W-RESOLVE-1 | Resolution follows the exact 6-step order; one overlay DB read covers steps 2–4 | §8 | §3 |
| W-RESOLVE-2 | `readdir` is O(direct children), returns names+d_type only, fetches 0 blobs | §4.5, §15, §38.2 | §3 |
| W-BASELINE-1 | Index-only ops (`reset --mixed`, `restore --staged`, `rm --cached`) leave working bytes & listing unchanged | §8.1, §25.1 | §4 |
| W-ADV-1 | Baseline advances only after a confirmed worktree-updating command; never from a changed index | §8.2, §25.2 | §5 |
| W-ADV-2 | Advancement preserves concurrently-written overlay entries | §8.2, §32.2 | §5.3 |
| W-ADV-3/4 | Dematerialization only when content+mode match, no writable handle/pending fsync/rename; never by timestamp | §8.2 | §5.4 |
| W-PATH-1 | Non-UTF-8 / newline / tab / dash / quote paths resolve, rename, tombstone, and round-trip in JSON/logs | §31 | §6 |
| W-RENAME-1/2 | Clean file and clean subtree rename fetch 0 (descendant) blobs | §29, §38.9 | §7 |
| W-RENAME-3 | Rename across an attribute boundary re-evaluates the filter context | §29, §23 | §7.3 |
| W-RENAME-4 | `RENAME_NOREPLACE`/`EXCHANGE`/case-only/open-handle renames behave per §29 | §29 | §7.1 |
| W-SYNTH-1 | Root `.git` is correct, protected, and wins over a colliding tree entry | §6 | §8 |
| W-LINK-1/2/3 | Symlinks byte-exact & unfiltered; hard links/special files refused cleanly; no symlink-swap follow | §30, §36 | §9 |

All marked invariants become differential tests against a conventional checkout
(§40.1) executed through a **real `/dev/fuse` mount** (§40.2), and the
hydration-budget ones (W-RESOLVE-2, W-RENAME-1/2) become automated fetch-count
assertions (§38).
