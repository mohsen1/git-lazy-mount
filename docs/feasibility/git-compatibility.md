# Feasibility: stock-Git compatibility

**Question.** What stock-Git behavior can we rely on, and which commands are
safe against a virtual workspace (spec §5.4)?

## Findings

* **Tree object serialization (release-relevant).** Hashing a tree via
  `git hash-object -t tree` requires the exact canonical byte stream. A subtree
  entry's mode must be `40000`; the zero-padded `040000` is rejected:
  `error: object … fails fsck: zeroPaddedFilemode`. Caught by differential
  testing; fixed in `GitMode::as_octal()` and guarded by
  `commit_reuses_subtrees_and_passes_fsck` (runs `git fsck`).
* **Commit/push interoperate.** Tree built from `base ⊕ staged-delta` (unchanged
  subtrees reused) → `commit-tree` → `update-ref` (CAS) → `push
  --force-with-lease`. A bare remote receives an ordinary commit and the file
  reads back from it (CLI e2e test). No custom server is involved.
* **Bare-store filtering needs `--attr-source`.** `cat-file --filters` resolves
  `.gitattributes` from `HEAD` by default; in a shared bare store whose `HEAD`
  need not match the workspace base, we pass `--attr-source=<base-commit>` so
  attributes are correct (verified by the CRLF test). See
  `feasibility/file-metadata.md`.
* **CAS primitives.** `update-ref <ref> <new> <old>` and `push
  --force-with-lease=<ref>:<expected>` both detect a concurrent move and are
  classified into `concurrent_branch_movement` (tested).

## Command classification (initial)

Based on these findings and spec §37, commands are classified as:

* **native-safe / read-only-safe today:** the lazy-mount `status/diff/add/
  unstage/restore/commit/push/branch-lease` plus read-only `log/show/rev-parse/
  cat-file` against the backing store.
* **wrapper-required (future):** mutating stock commands (`checkout/switch/
  reset/merge/rebase/clean`) need the temp-index transaction wrapper + a
  hydration-budget test each before being advertised.
* **experimental / unsupported:** a raw `.git` facade (Level D) and `fsmonitor`
  authority are not exposed.

See `docs/design/compatibility.md` for the living matrix. We do not advertise a
command until a test proves it.

## Not yet measured

The behavior of running stock `git status`/`add`/`commit` *through a real mount*
(lstat counts, whether Git walks the full tree, skip-worktree/sparse-index
assumptions) requires the kernel backend and is deferred until the FUSE adapter
lands.
