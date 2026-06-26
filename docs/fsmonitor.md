# FSMonitor v2 protocol + the durable change journal

This doc is the canonical reference for how git-lazy-mount answers Git's
FSMonitor v2 queries and for the **zero-blob first `git status`** seed. It is
part of the [specification](design.md).

The shape is small. Stock `git`, invoked directly inside the mount, calls our
hook binary; the hook opens a durable append-log journal the serve process
writes and replies with the paths that changed. There is **no daemon, no IPC, no
socket, no SQLite** — just one hook binary and one log file.

Two pieces implement it:

- the hook binary
  [`crates/cli/src/bin/fsmonitor_hook.rs`](../crates/cli/src/bin/fsmonitor_hook.rs)
  (installed as `git-lazy-mount-fsmonitor`), wired to `core.fsmonitor`; and
- the token + journal types in
  [`crates/worktree/src/journal.rs`](../crates/worktree/src/journal.rs).

## Where this fits

| Owner | Responsibility |
|-------|----------------|
| **Git** (native admin gitdir) | calls our `core.fsmonitor` hook; owns `.git/index` FSMonitor-valid bits, `HEAD`, refs |
| **serve process** (the FUSE host) | records every worktree mutation into `<gitdir>/glm-fsmonitor/changes.log` synchronously, before each FUSE reply |
| **hook** (`git-lazy-mount-fsmonitor`) | opens that same log and answers the FSMonitor query; no `git`, no worktree scan, no IPC |

The serve process and the hook never talk to each other. They agree because they
derive the same workspace id and journal path from the gitdir, and the hook reads
the log the serve process already fsynced. The log file is the only channel.

### Config written at mount

`configure_fsmonitor`
([`crates/cli/src/main.rs`](../crates/cli/src/main.rs)) sets exactly two keys,
via plain `git config`:

```
core.fsmonitor            = <dir-of-exe>/git-lazy-mount-fsmonitor
core.fsmonitorHookVersion = 2
```

Nothing else. There is no `core.untrackedCache` and no `core.hooksPath` — Git's
default hook path and untracked cache are left untouched. `core.fsmonitor`
points at the hook binary that ships next to `git-lazy-mount`; if it is not found
beside the running executable, FSMonitor is simply not configured (the mount
still works, just with eager `git status`).

---

## 1. The FSMonitor v2 wire protocol (Git ↔ hook)

This is Git's contract, which our hook implements toward Git.

**Request** (argv to the hook): `argv[1] = "2"` (version), `argv[2] = <prev
token>` — an opaque string we minted on a previous query, or empty `""` on first
use. Any `argv[1]` other than `"2"` gets a full invalidation.

**Response** (hook stdout, exactly):

```
<new-token> NUL <path1> NUL <path2> NUL ... <pathN> NUL
```

- The leading token is everything up to the **first NUL**.
- After it, zero or more **NUL-separated, repo-root-relative** paths.
- Paths use `/`; bytes are emitted verbatim, never lossy-UTF-8. The journal
  stores raw path bytes
  ([`crates/core/src/path.rs`](../crates/core/src/path.rs)) and writes them
  straight to stdout.
- The set is **inclusive**: it must contain every path that *might* have changed
  since `prev token`. False positives are acceptable; false negatives are never
  acceptable. A returned path that did not actually change only costs Git an
  extra `lstat`. A missing path corrupts `git status`.

`Query::encode`
([`crates/worktree/src/journal.rs`](../crates/worktree/src/journal.rs))
serializes both the change-set reply and the full-invalidation sentinel.

### 1.1 Full-invalidation sentinel

When the hook cannot prove continuity from `prev token` to now, the response is a
single path `/`:

```
<new-token> NUL / NUL
```

`/` tells Git to treat the entire worktree as possibly-changed and rescan (it
clears all FSMonitor-valid bits and stats everything). This is always correct but
eager. It is the safety valve, never a steady state. The hook also prints `/`
fail-safe on any internal error (missing journal, parse failure) and always exits
0 — Git treats a nonzero exit as "rescan everything" anyway, and we never want
garbage on stdout paired with a nonzero status.

---

## 2. Token identity

A token is opaque **to Git** but **structured for the journal**. The wire form
([`Token::encode`/`Token::parse`](../crates/worktree/src/journal.rs)) is:

```
glm1:<workspace>:<epoch>:<seq>:<generation>
```

- `glm1` is a format tag; a token without it (or with the wrong field count) →
  full invalidation.
- `workspace` is a stable id derived by hashing the admin gitdir path
  (`workspace_id`). The serve process and the hook derive it identically, so
  their tokens match with no shared metadata file. A token from a different
  workspace → full invalidation.
- `seq` is the journal's monotonic record count at mint time. Each recorded
  change is one increment.
- `epoch` and `generation` are **fixed at `1` and `0`** everywhere — the CLI
  seed, the serve process, and the hook all call `ChangeJournal::open(.., 1, 0)`.
  They are carried in the token (and compared on query) so a future incarnation
  can start using them without a wire-format change, but today they never move.

A reset or truncated log does not need an epoch bump to stay correct: a token
whose `seq` exceeds the current journal length is rejected as a future seq (see
below), so a shorter-than-expected log degrades to a safe full invalidation
rather than a false negative.

The journal is not compacted, so its log grows unbounded over a long-lived
mount; `epoch` and `generation` are fixed at `1` and `0`.

---

## 3. The query algorithm

`ChangeJournal::query(prev)`
([`crates/worktree/src/journal.rs`](../crates/worktree/src/journal.rs)) is the
whole server side. It holds no locks across I/O and does no `git` call. With
`cur_seq` = the number of records replayed from the log:

1. **Empty `prev`** (first use): if `cur_seq == 0`, return an **empty** change
   set (the bootstrap — see §4). If `cur_seq > 0`, return full invalidation: the
   hook cannot prove continuity from a token it never issued.
2. **Unparseable / wrong-tag `prev`** → full invalidation.
3. **Workspace mismatch** (`token.workspace != current`) → full invalidation.
4. **Epoch mismatch** (`token.epoch != 1`) → full invalidation.
5. **Generation mismatch** (`token.generation != 0`) → full invalidation.
6. **Future seq** (`token.seq > cur_seq`) → full invalidation (a shorter log than
   the token implies, e.g. after an unexpected reset).
7. Otherwise return the **inclusive, sorted, deduplicated** set of recorded paths
   with `token.seq < seq <= cur_seq`, plus a fresh token at `cur_seq`.

That is the complete list of full-invalidation branches — there is no rollback
detection, no compaction floor, no future-generation-vs-delta logic, no
queue-overflow or reconcile-on-restart state. Those do not exist in the code.
These branches are exercised by `full_invalidation_on_unplaceable_tokens` and
`changes_since_token_are_inclusive` in the `journal.rs` test module.

### What gets recorded

The serve process records a path on every worktree-mutating FUSE op (create,
write, truncate, unlink, mkdir, rmdir, symlink, and both endpoints of a rename),
via `Projection::record_change`
([`crates/worktree/src/lib.rs`](../crates/worktree/src/lib.rs)). For a path
whose parent directory's listing changes, the parent is recorded too, so Git's
directory-level checks see it. Recording is **synchronous** (`record` does
`write_all` + `sync_data`) and happens **before** the FUSE reply — and before the
mutation is applied. A journal write failure **fails the FUSE op** rather than
applying an un-journaled change, so the log can never miss an acknowledged
mutation (no false negatives). Over-reporting is harmless because the set is only
required to be inclusive.

Branch-changing commands (`switch`/`checkout`/`reset --hard`/`merge`/`rebase`)
flow through this same FUSE write path: stock git writes each changed path, so
each is recorded. See [compatibility.md](compatibility.md) for the
per-command laziness matrix.

The log is replayed into an in-memory `Vec` and **kept whole** — there is no
compaction, so a very long-lived mount with many mutations grows the log and the
replayed vector without bound. Because `record` is synchronous and precedes the
reply, a `git status` issued after an editor's `write()` returned already sees
that path.

---

## 4. Bootstrap: the first `git status` is zero-blob

This is the canonical description of the seed. [limitations.md](limitations.md)
(item P1) and the `query()` doc-comment summarize it; this section is the source
of truth.

The **first** clean `git status` faults **zero** blobs, the same as every repeat.
A freshly `read-tree`'d index carries **no FSMonitor extension**, so git's "mark
every entry valid" pass (which runs only when the extension is read from disk)
never runs on the first status. Without it, git stats every entry — and under a
lazy clone, `getattr` would fault each blob for its size — and only *then* writes
the extension. The hook's "nothing changed" reply cannot help, because the valid
bits were never set going in. An early `GIT_TRACE_FSMONITOR` reading was misread
as a fundamental limit; it was a **bootstrap-ordering** problem.

The fix is to **pre-seed** the extension at mount, right after `read-tree`
([`AdminRepo::seed_fsmonitor_valid`](../crates/git-repo/src/lib.rs)): pipe every
tracked path through `git update-index -z --fsmonitor-valid --stdin`, which sets
each entry's `CE_FSMONITOR_VALID` bit and records the hook's current (seq-0)
token. The seed runs only after the empty journal exists, so the hook can answer
the bootstrap query. Then on the first status:

1. Git loads the extension and trusts every entry's valid bit.
2. Git calls the hook with the seeded token; the journal is still at `seq 0`
   (no worktree write recorded since the index was built), so the empty-`prev`
   bootstrap path returns an **empty** change set with a fresh token.
3. Git's `refresh_cache_ent` early-returns on `CE_FSMONITOR_VALID` for every
   entry, **before** any `lstat`/size/content check. Zero `lstat`, zero blob
   faults.

This is sound because the serve process **owns the filesystem** and knows no
worktree write has occurred since the index was built (any write increments
`seq`). The moment a FUSE write advances `seq`, the hook's reply includes the
affected paths and git rechecks exactly those. An empty `prev` once `seq > 0`
returns `/`: the hook cannot prove continuity from a token it did not issue.

Verified zero-fault on a small repo by
`first_status_faults_zero_blobs_and_surfaces_edits`
([`crates/cli/tests/fsmonitor.rs`](../crates/cli/tests/fsmonitor.rs)). The
~81k-file figure is a separate manual measurement on the
microsoft/TypeScript mount (README performance table), not from this test.

### Conversion-attribute carve-out (all-or-nothing)

A path under a checkout-conversion attribute — a clean/smudge `filter=`,
`ident`, `working-tree-encoding=`, or CRLF `eol=crlf` — reads through the mount
as the raw baseline blob, which can differ from a real checkout, so seeding it
valid could hide a real diff (this is limitation [R7](limitations.md)).

The carve-out is **all-or-nothing, not per-path**: if any tracked
`.gitattributes` declares any such attribute (`declares_conversion_attributes`
reads the `.gitattributes` blobs directly, each bounded by
`SEED_ATTR_READ_TIMEOUT_SECS = 20`), `seed_fsmonitor_valid` **skips the entire
seed** and the first status falls back to the eager scan — which is correct, just
not optimized. The common case (no conversion attribute) seeds every entry.

`ls -l`/`stat` of an unmaterialized file still faults its blob once for the exact
size — that is limitation [R6](limitations.md), a `getattr` cost separate from
`git status`, which does not stat seeded entries.

---

## 5. Why the hook touches no worktree

Git spawns the hook from inside the mount. The hook must therefore **not** read
worktree content: it resolves the admin gitdir from the synthetic `.git` gitfile
(falling back to `GIT_DIR`, then `git rev-parse --absolute-git-dir`), opens the
journal there, and answers. No `git status`, no filters, no hydration — the query
path cannot fault a blob or run a smudge filter. The synthetic `.git` gitfile
holds the exact admin-dir path the CLI wrote, the same one the serve process
uses, so the workspace id and journal path always agree across calls. See
[deadlock-startup-recovery.md](deadlock-startup-recovery.md) for why the
FSMonitor query path must stay off the worktree.

---

## Related

- Token + journal types and tests:
  [`crates/worktree/src/journal.rs`](../crates/worktree/src/journal.rs).
- The hook binary:
  [`crates/cli/src/bin/fsmonitor_hook.rs`](../crates/cli/src/bin/fsmonitor_hook.rs).
- Overlay durability and the atomic-sidecar fsync discipline (a sibling of the
  journal's synchronous-record rule): [durability-security.md](durability-security.md).
- The `read-tree HEAD` index build the seed runs against, and the throwaway
  operational-index interop bridge
  ([`crates/git-store/src/interop.rs`](../crates/git-store/src/interop.rs), which
  still exists and is exercised by `store_integration.rs`):
  [index-strategy.md](index-strategy.md).
- Per-command compatibility and laziness: [compatibility.md](compatibility.md).
