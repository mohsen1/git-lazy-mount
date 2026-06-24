# Redesign requirements checklist (Linux MVP)

Living tracker for [`redesign.md`](../../redesign.md). Updated as items pass
through a **real mount in CI**. `[ ]` = not done, `[~]` = in progress / partial,
`[x]` = done + tested. Nothing is checked until proven by a mounted test.

## A. Vertical-slice experiments (§39) — gate before broad work

- [x] **A** real mounted `.git`: `git -C <mnt> rev-parse --show-toplevel` → `<mnt>` — *CI `redesign linux mount`*
- [~] **B** zero-content readdir: `ls` fetches **0** blobs (proven via hydration counter; 100k-file *scale* stress test pending)
- [ ] **C** transparent edit + status: edit then `git status --porcelain=v2` correct, no wrapper *(needs M2 overlay + M3 index)*
- [ ] **D** real staging: `git add` then `git diff --cached` uses the real index
- [ ] **E** interactive staging: `git add -p` (real PTY) stages one hunk
- [ ] **F** real commit: `git commit` / `--amend`, no adoption step
- [ ] **G** checkout behavior: measure stock-git eagerness over a 100k-file delta
- [ ] **H** FSMonitor bootstrap: first + subsequent clean status read 0 working blobs
- [ ] **I** large-file I/O: multi-GiB blob, bounded memory, no full rewrite per write

## B. Linux MVP release criteria (§43) — all via a real mount

- [~] 1 `git lazy-mount <url> <path>` clones + mounts + validates (mount proven; the one-command CLI lifecycle is the next M1 step)
- [x] 2 no required shell env changes afterward — stock `git` used directly in CI
- [x] 3 `git rev-parse --show-toplevel` → mountpoint — *CI*
- [x] 4 normal `.git` gitfile → native admin dir — *CI*
- [x] 5 plain `ls` fetches no file blobs — *CI (hydration counter == 0)*
- [x] 6 reading one missing file fetches no unrelated blobs — *CI (cat hydrates exactly 1)*
- [ ] 7 editor atomic save updates the overlay correctly
- [ ] 8 plain `git status` sees the edit
- [ ] 9 plain `git add` stages it in the real index
- [ ] 10 plain `git add -p` stages selected hunks
- [ ] 11 plain `git commit` advances a normal branch directly
- [ ] 12 plain `git commit --amend`
- [ ] 13 plain `git push` to an ordinary remote
- [ ] 14 plain `git fetch` / `git pull`
- [ ] 15 plain `git switch` correct + hydration measured
- [ ] 16 merge conflicts use the real index conflict stages
- [ ] 17 rebase abort + continue
- [ ] 18 stash create + restore
- [ ] 19 `git rm --cached` preserves the working-tree file
- [ ] 20 `git reset --mixed` changes index without changing projected bytes
- [ ] 21 `git reset --hard` replaces projected working state
- [ ] 22 open-unlink semantics
- [ ] 23 empty untracked directories survive remount
- [ ] 24 partial writes don't rewrite the full file per callback
- [ ] 25 multi-GiB files don't require multi-GiB allocations
- [ ] 26 dirty state survives unmount/remount
- [ ] 27 dirty state survives an injected daemon crash
- [ ] 28 FSMonitor survives restart or safely requests full invalidation
- [ ] 29 no command requires `git lazy-mount git --`
- [ ] 30 no ordinary workflow requires custom add/commit/switch/push

## C. Anti-claims (§44) — must NEVER be true at "done"

- [ ] registry says mounted without a kernel mount
- [ ] plain Git cannot discover the repository
- [ ] a temporary gitdir generated per command
- [ ] commits imported after Git exits
- [ ] custom stage differs from `.git/index`
- [ ] status only works through a wrapper
- [ ] push only works through a bespoke command
- [ ] `ls` hydrates every file in a directory
- [ ] read allocates the complete blob
- [ ] each write rewrites the complete file
- [ ] open handles are path lookups in disguise
- [ ] open-unlink fails
- [ ] empty directories vanish immediately
- [ ] one FUSE callback creates one OS thread
- [ ] FSMonitor state disappears silently on restart
- [ ] Git paths converted lossily to UTF-8
- [ ] shared-cache maintenance invalidates active workspaces
- [ ] another platform called supported without a real mount test

## D. Hydration budgets (§38) — automated assertions

- [x] mount (blob:none) fetches 0 working-file blobs to project the tree — *CI*
- [x] `ls <dir>`: 0 child blobs, 0 smudge filters, O(direct children) — *CI (small scale)*
- [ ] clean `git status` (post-bootstrap): 0 blobs, 0 smudge, no full stat *(M3)*
- [x] `cat path`: ≤ the one required blob + its attr/filter metadata — *CI*
- [ ] 100 concurrent reads of one missing file → 1 retrieval
- [ ] `O_TRUNC` open: no old-blob fetch
- [ ] 4 KiB writes to a 1 GiB file: no full read/rewrite, no GiB allocation
- [ ] clean rename of unmaterialized file: 0 blob fetches
- [ ] `git log`/`branch`/`tag`/`status`: no working-blob hydration

## Process (this goal)

- Linux-only focus; runs **fully in Linux CI** through a real `/dev/fuse` mount.
- Every commit self-reviewed (reviewer teammate on substantial diffs).
- Differential tests vs a conventional checkout for every workflow (§40.1).
- The compatibility report (§3, §40.3) is generated from test results.
