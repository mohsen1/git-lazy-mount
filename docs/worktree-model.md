# Virtual working-tree model: baseline + overlay + journal

This document describes how `git-lazy-mount` computes the bytes of the virtual
working tree: the rules for unmaterialized vs. locally-written content, rename
semantics, symlinks / hard links / special files, the protected synthetic
`.git`, and raw repository paths. It is the canonical home for the projection
model; the FUSE op set lives in [fuse-semantics.md](fuse-semantics.md), object
fetching in [object-fetching.md](object-fetching.md), and the durable change log
in [fsmonitor.md](fsmonitor.md). The overall spec is [design.md](design.md).

This document covers what bytes a path *contains*. It does **not** specify
staging, commit, refs, or status — those are Git's, served from the real
`$GIT_DIR/index` and refs. The implementation is the `glm-worktree` crate
(`crates/worktree/src/{lib.rs,overlay.rs,journal.rs}`).

---

## 1. Scope and the two-source-of-truth boundary

| Owner | State governed |
|-------|----------------|
| **Git** (the admin gitdir) | `HEAD`, refs, reflogs, the real index + conflict stages, commit/merge/rebase state. |
| **Projection** (this doc) | The *working-tree bytes*: baseline + overlay + tombstones + the synthetic `.git`, plus inode identity and rename mappings. |

The working tree is a **pure projection** layered from a fixed baseline tree, the
durable overlay, and Git objects. It answers exactly one question:

> What bytes would this path contain in the logical working tree, right now?

It never answers "what is staged", "what is HEAD", or "what branch is checked
out"; those flow from Git. The projection's parse of a Git tree is a disposable
cache. The overlay is **not**: it holds acknowledged user bytes and is durable.

The implemented type is `Projection` (`crates/worktree/src/lib.rs`). Its inputs:

```rust
pub struct Projection {
    repo: AdminRepo,            // the admin gitdir + object store
    inodes: InodeTable,         // crates/fs-common — stable identity, rename, open-unlink
    baseline_tree: ObjectId,    // the HEAD commit's tree, FIXED at open (see §2)
    gitfile: Vec<u8>,           // the single synthetic `.git` content
    overlay: Overlay,           // crates/worktree/src/overlay.rs — durable local state
    cache_dir: PathBuf,         // content-addressed materialized blobs
    hydrations: AtomicU64,      // cache-miss counter (the fetch-budget signal)
    metadata_fetch: bool,       // whether getattr faults a blob for exact size
    inflight: Mutex<HashMap<ObjectId, Arc<Mutex<()>>>>, // per-oid single-flight
    journal: Option<journal::ChangeJournal>, // FSMonitor durable log (§5)
}
```

There is no global lock: the `InodeTable`, `Overlay`, and content cache are each
internally synchronized, so callbacks never serialize behind a coarse mutex held
across a blocking `git` subprocess.

---

## 2. The model: baseline + overlay + tombstones + synthetic `.git`

```
working_tree(path) = resolve(synthetic, overlay, baseline)
```

The three sources:

1. **Synthetic `.git`**: the one reserved control entry — a read-only regular
   file at the root, protected from unlink/rename/replace/chmod/write/mkdir-
   beneath. Content is exactly `gitdir: <abs path to admin gitdir>\n`, served
   from a fixed buffer.
2. **Overlay** (`crates/worktree/src/overlay.rs`): the durable writable layer of
   *locally materialized* state — written files, created symlinks, created
   directories, **base-refs** (a clean rename pointing at an existing blob with
   no bytes copied), and **tombstones** (deletions). Each entry is one atomic
   JSON sidecar plus, for files, a native content file. See
   [durability-security.md](durability-security.md) for the persistence format.
3. **Baseline**: the committed Git **tree** that an unmaterialized path reads
   from. Lazy — nothing is fetched merely to hold a baseline.

`baseline_tree` is set once at `Projection::open` from `repo.head_tree()` and is
**immutable for the life of the projection** (`lib.rs:208-214`); it is fixed at
projection open and does not advance in-process. Branch-changing Git commands
(switch/checkout/merge/rebase) still work and
stay correct because stock Git writes every changed path through the FUSE write
path, landing in the overlay — they do not need the projection to move its
baseline.

---

## 3. Path resolution order

`Projection::resolve(path)` (`lib.rs:302-336`) returns a `Resolved`
(`Dir`/`File`/`Symlink`/`Gitfile`). The order, top to bottom:

| # | Source | Condition | Result |
|---|--------|-----------|--------|
| 1 | **Synthetic `.git`** | the path is exactly the root `.git` | `Gitfile`. Shadows any baseline/overlay entry of the same name; a repo tree that contains a literal root `.git` is suppressed, never projected. |
| 2 | **Overlay entry** | `overlay.lookup(path)` is `File`/`Symlink`/`Dir`/`BaseRef` | local bytes (`File`/`Symlink`), an overlay directory, or a `BaseRef{oid,mode}` that streams the referenced baseline blob with **no fetch to relocate**. |
| 3 | **Overlay tombstone** | `overlay.lookup(path) == Tombstone`, or any proper ancestor is tombstoned | `None` (absent) — the deletion shadows the baseline. |
| 4 | **Baseline tree** | `baseline_resolve(path)` finds an entry | walks the tree component-by-component (one tree object per level), yielding the file/symlink/dir there. |
| 5 | **Absent** | none of the above | `None`. |

Notes:

- Steps 2 and 3 are **one** overlay lookup; the entry kind dispatches.
- The baseline walk reads only **tree** objects, never blob contents. Tree
  objects for the committed baseline are fetchable/present after the default
  `tree:0` partial clone (`crates/git-repo/src/lib.rs:50`), which `build_index`
  faults at mount via `read-tree HEAD`. (A `blob:none` clone would instead
  download every tree from all of history — slow and large; `tree:0` keeps the
  baseline cheap. See [index-strategy.md](index-strategy.md).)
- **Implied / overlay directories**: a created `a/b/c` makes `a/b` a directory
  because `mkdir`/`create` records the parent overlay `Dir` entries; resolution
  and `readdir` then merge them with the baseline.

`readdir(dir)` (`lib.rs:484-552`) is the union of (a) the baseline tree's direct
children and (b) overlay children whose parent is `dir`, with tombstones
subtracting and overlay files/base-refs adding. It is **O(direct children)**,
returns names + kind + inode only — **never** sizes or blob reads — and at the
root suppresses the baseline `.git` in favor of the synthetic one. (`ls -l`
faults a blob per file for its exact size; that is `getattr`, not `readdir` —
see [object-fetching.md](object-fetching.md).)

---

## 4. Why a fixed baseline is safe for index-only commands

An index-only Git operation changes `$GIT_DIR/index` **without** touching
working-tree bytes:

```bash
git reset --mixed <commit>      # index ← <commit> tree; worktree unchanged
git restore --staged <path>     # index entry ← HEAD; worktree unchanged
git rm --cached <path>          # index entry removed; worktree FILE STAYS
git update-index --cacheinfo …  # index entry forced; worktree unchanged
```

The projection never sources unmaterialized content from the index — only from
`baseline_tree` and the overlay — so none of these touch what the mount serves.
A working-tree edit goes to the **overlay**, never the index; the index changes
only when the user runs `git add`, which Git does through the real index. The
two move independently and correctly.

---

## 5. The change journal (FSMonitor durable log)

The third pillar of the crate is the `ChangeJournal`
(`crates/worktree/src/journal.rs`), the durable record that lets stock `git
status` stay fast. It is canonically documented in [fsmonitor.md](fsmonitor.md);
the projection's responsibility is to **record every mutation before
acknowledging it**.

- The journal is a NUL-separated **append log** at
  `<gitdir>/glm-fsmonitor/changes.log`, replayed into an in-memory `Vec` on open.
- Every mutating handler (`create`, `write`, `truncate`, `unlink`, `rmdir`,
  `mkdir`, `rename`, `symlink`) calls `record_change(path)` (`lib.rs:250-260`)
  **before** applying the mutation. It records the path and its parent
  directory; `record()` is synchronous (`write_all` + `sync_data`) **before the
  FUSE reply**. Over-reporting is safe (git just `lstat`s an unchanged path); a
  missing record would be a false negative for `status`, so a journal write
  failure **fails the FUSE op** rather than applying an un-journaled mutation
  (test `journal_record_failure_fails_the_mutation_not_silently_succeeds`,
  `lib.rs:1111`).
- The FSMonitor hook reads the same log and answers git's
  `(version, previous_token)` query. The token wire form is
  `glm1:ws:epoch:seq:gen`; `epoch` and `generation` are fixed at `1` and `0`
  (`journal.rs:36-43`). Continuity it cannot prove yields full invalidation (`/`).

---

## 6. RepoPath: raw bytes as identity

Paths are `RepoPath` (`crates/core/src/path.rs`): identity is the **raw bytes**,
never lossy UTF-8.

| Property | Mechanism |
|----------|-----------|
| No lossy UTF-8 for identity | `as_bytes()` is the only identity; `Hash`/`Eq`/`Ord` are over bytes. `display()` is lossy and explicitly *not* parseable back. |
| NUL-delimited plumbing | Git invocations use NUL framing; `RepoPath` forbids embedded NUL, so a path can never inject a delimiter. |
| Safe display / JSON escaping | `escape()`/`unescape()` round-trip; `serde` uses `escape()`, so non-UTF-8 paths survive the overlay sidecars and the journal log. |
| Object access by oid | Content streams by **oid**, never `rev:path`, so resolution never builds a `rev:path` argument. |
| Byte-wise traversal | Resolution walks `components()` (byte slices); it never UTF-8-decodes to traverse. |

**Rejected on construction** (unit-tested in `from_bytes`): embedded `NUL`,
absolute (`/`-leading), `.`/`..` traversal, empty components (`a//b`).

### 6.1 The non-UTF-8 filter case

`GitStore::smudge_blob` passes `--path=<utf8>` to `git cat-file --filters` and
errors on non-UTF-8 paths. The projection sidesteps this by serving the **raw
baseline blob** directly — no plumbing path argument needed, correct and
byte-exact. A smudge-filtered file (eol=crlf, ident, an LFS pointer) therefore
reads as its stored bytes, not the smudged bytes; commits stay byte-correct
because the clean filter is the inverse. This is by design and is what lets
non-UTF-8 paths read unfiltered. See [object-fetching.md](object-fetching.md)
and [limitations.md](limitations.md).

Test `pathological_names_roundtrip` (`lib.rs:1505`) exercises a path with
invalid UTF-8, a newline, a tab, a leading dash, a backslash, and quotes —
created, read back, and listed correctly.

---

## 7. Rename semantics

A rename changes the *namespace*, not necessarily content. `Projection::rename`
(`lib.rs:871-927`) preserves inode identity and fetches **no** blob for a clean
rename. It honors `RENAME_NOREPLACE` (fail with `EEXIST` if the destination
exists) and **rejects `RENAME_EXCHANGE`** as unsupported (`UnsupportedOperation`
→ `EOPNOTSUPP`).

### 7.1 Clean rename = metadata only

The key mechanism is the overlay `BaseRef { oid, mode }`
(`crates/worktree/src/overlay.rs`): a reference to the **existing blob** placed
at the new path, copying or fetching no bytes.

| `from` resolves to | Action at `to` | Fetch? |
|--------------------|----------------|--------|
| Overlay `File`/`Symlink` | re-key the content record to `to` (`overlay.rename`); tombstone `from` if it also shadows a baseline entry | none |
| Baseline file/symlink | `put_base_ref(to, oid, mode)`; tombstone `from` | none |
| Baseline/overlay **directory** | move the whole subtree, §7.2 | none |
| Tombstone / Absent | `ENOENT` | n/a |

The inode moves with the content (`InodeTable::rename`), so open handles on
`from` keep working. Test `rename_rekeys_overlay_content_and_preserves_inode_identity`
(`lib.rs:1374`).

### 7.2 Clean subtree rename = no descendant reads

A whole-directory rename (`rename_dir`, `lib.rs:933-975`) is metadata-only:
overlay descendants re-key (content moves with them), baseline descendants become
`BaseRef`s at the destination, and the source subtree is tombstoned so the
baseline beneath it is hidden. **No descendant blob is read.** Test
`directory_rename_moves_subtree_without_fetch` (`lib.rs:1579`) asserts a
zero-blob hydration budget across an N-file directory.

---

## 8. The protected synthetic `.git`

The only reserved control entry is the root `.git` regular file. It is protected
**by path**, not by a reserved inode, and the rejection comes from two distinct
places. **Name-based** mutations — `unlink`, `rename` (source or dest), `create`,
and `mkdir` beneath it — go through `child_path` (`lib.rs:649-666`), which rejects
any target that is exactly the root `.git` with `ErrorCode::Authentication` →
`EACCES`. **Inode-based** mutations never reach `child_path`: they fail because
`.git` resolves to a non-writable `Gitfile`, so `write` (`open_write`,
`lib.rs:709-722`) and `truncate` (`lib.rs:740`) return `ErrorCode::Internal`
("not a writable file" / "not a file"), and `chmod`/`setattr`-executable
(`set_executable`, `lib.rs:1037`) returns `ErrorCode::Internal` ("not a file").

At resolution it wins over any baseline or overlay entry of the same name: a Git
tree that contains a literal root `.git` entry is suppressed and never projected,
so a malicious or pathological repo cannot overwrite Git's protected namespace.
Test `synthetic_git_is_a_single_protected_regular_file_at_root` (`lib.rs:1175`)
only asserts that `.git` is listed once as a regular file with `gitdir:` content;
the mutation-rejection ("protected") aspect is covered by the mount integration
tests, not this unit test.

---

## 9. Symlinks, hard links, special files

### 9.1 Symlinks

Git symlinks (`GitMode::Symlink`, blob bytes = target) project as **native
symlinks**. Overlay symlinks store the raw target bytes (`overlay.put_symlink`);
baseline symlinks stream the blob unfiltered.

- **Raw target bytes are preserved exactly**: broken, relative, absolute, and
  non-UTF-8 targets all round-trip. Symlink blobs are **never filtered** (test
  `readlink_returns_raw_symlink_target`, `lib.rs:1243`).
- **The projection never follows a repo symlink for its own storage**: the
  overlay meta sidecar is addressed by a hash of the path bytes (`id_for` =
  `sha256(path).json`), and content files use a per-process unique id
  (`new_content_id` = `c{pid}-{seq}`), not a path hash — so it never `open`s a
  repo-controlled path for internal writes. This structurally avoids the
  symlink-swap TOCTOU class.

### 9.2 Hard links and special files

Neither `link` nor `mknod` is implemented in the FUSE layer
(`crates/fuse/src/mount.rs`), so the fuser default applies and both return
**`ENOSYS`**. Git preserves neither hard-link identity nor device/fifo/socket
nodes (no such `GitMode`), so refusing them keeps everything the projection
serves representable as Git content. If ever supported they would be overlay-only
and explicitly non-committable. See [limitations.md](limitations.md).

---

## 10. Tested invariants

The model's guarantees are covered by tests in `crates/worktree/src/lib.rs` and
by the differential / hydration-budget tests run against a real `/dev/fuse`
mount:

| Guarantee | Test |
|-----------|------|
| Resolution order; overlay over baseline; tombstone masks descendants | `unlink_baseline_tombstones_and_hides_from_readdir`, `ancestor_tombstone_masks_an_untombstoned_child` |
| `readdir` is O(direct children) and fetches 0 blobs | `large_directory_readdir_fetches_zero_blobs` |
| CoW edit reads back merged; copy-up once | `cow_edit_of_a_baseline_file_reads_back_merged`, `set_executable_on_baseline_file_copies_up` |
| Clean file/dir rename fetches 0 blobs; inode identity preserved | `mkdir_symlink_and_clean_rename_without_fetch`, `rename_rekeys_overlay_content_and_preserves_inode_identity`, `directory_rename_moves_subtree_without_fetch` |
| Raw / pathological paths round-trip | `pathological_names_roundtrip` |
| Synthetic `.git` is single, protected, and wins (the unit test asserts only single + regular file + gitfile content; mutation-rejection is covered by the mount integration tests) | `synthetic_git_is_a_single_protected_regular_file_at_root`, `root_readdir_lists_tree_entries_plus_synthetic_git` |
| Symlink targets byte-exact and unfiltered | `readlink_returns_raw_symlink_target` |
| Journal records before acknowledging; failure fails the op | `journal_record_failure_fails_the_mutation_not_silently_succeeds` |
| Overlay matches a reference model (property test) | `property_overlay_matches_a_reference_model` |
