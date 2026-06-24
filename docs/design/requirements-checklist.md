# Design requirements checklist (Linux MVP)

Living tracker for [`design.md`](../../design.md). Updated as items pass
through a **real mount in CI**. `[ ]` = not done, `[~]` = in progress / partial,
`[x]` = done + tested. Nothing is checked until proven by a mounted test.

## A. Vertical-slice experiments (§39) — gate before broad work

- [x] **A** real mounted `.git`: `git -C <mnt> rev-parse --show-toplevel` → `<mnt>` — *real mount*
- [x] **B** zero-content readdir: a 1000-file directory readdir fetches **0** blobs (`large_directory_readdir`); full 100k-file CI stress is a noted scale refinement
- [x] **C** transparent edit + status: edit then `git status --porcelain` correct, no wrapper — *real mount (`m3_git`)*
- [x] **D** real staging: `git add` then `git diff --cached` uses the real index — *real mount (`m3_git`)*
- [x] **E** interactive staging: `git add -p` stages one hunk — *real mount (`git_extra`, stdin-fed)*
- [x] **F** real commit: `git commit` + `--amend`, no adoption step — *real mount (`m3_git`/`git_more`)*
- [~] **G** checkout/switch: correct through the mount (`m4_m5`); eagerness over a 100k-file delta not yet *measured*
- [ ] **H** FSMonitor bootstrap: first + subsequent clean status read 0 working blobs *(M3 optimization)*
- [~] **I** large-file I/O: 4 MiB in-place 4 KiB writes proven (`m2_semantics`); multi-GiB bounded-memory pending

## B. Linux MVP release criteria (§43) — all via a real mount

- [x] 1 `git lazy-mount <url> <path>` clones + mounts + validates (no subcommand) — *real mount (`transparent_e2e`: the binary returns, then stock git works)*
- [x] 2 no required shell env changes afterward — stock `git` used directly
- [x] 3 `git rev-parse --show-toplevel` → mountpoint — *real mount*
- [x] 4 normal `.git` gitfile → native admin dir — *real mount*
- [x] 5 plain `ls` fetches no file blobs — *real mount (hydration counter == 0)*
- [x] 6 reading one missing file fetches no unrelated blobs — *real mount (cat hydrates 1)*
- [x] 7 editor atomic save updates the overlay correctly — *real mount (`m2_semantics` rename-over)*
- [x] 8 plain `git status` sees the edit — *real mount (`m3_git`)*
- [x] 9 plain `git add` stages it in the real index — *real mount (`m3_git`)*
- [x] 10 plain `git add -p` stages selected hunks — *real mount (`git_extra`, stdin-fed)*
- [x] 11 plain `git commit` advances a normal branch directly — *real mount (`m3_git`)*
- [x] 12 plain `git commit --amend` — *real mount (`git_more`)*
- [x] 13 plain `git push` to an ordinary remote — *real mount (`m4_m5`)*
- [x] 14 plain `git fetch` + merge — *real mount (`git_extra`; remote commit faults in over the promisor)*
- [x] 15 plain `git switch` correct — *real mount (`m4_m5`)*; hydration not yet measured
- [x] 16 merge conflicts use the real index conflict stages — *real mount (`git_extra`: stages 1/2/3 + overlay markers, §25.3)*
- [x] 17 rebase abort restores state — *real mount (`git_extra`)*; `--continue` flow not yet tested
- [x] 18 stash create + restore — *real mount (`git_more`)*
- [x] 19 `git rm --cached` preserves the working-tree file — *real mount (`git_more`)*
- [x] 20 `git reset --mixed` changes index without changing projected bytes — *real mount (`git_more`)*
- [x] 21 `git reset --hard` replaces projected working state — *real mount (`m4_m5`)*
- [x] 22 open-unlink semantics — *real mount (`m2_semantics`)*: fd reads/writes survive unlink; getattr falls back to the live fd (§17.4)
- [x] 23 empty untracked directories survive remount — *real mount (`m2_semantics`)*
- [x] 24 partial writes don't rewrite the full file per callback — *real mount (`m2_semantics` 4 KiB)*
- [~] 25 multi-GiB files don't require multi-GiB allocations — 4 MiB proven; multi-GiB pending
- [x] 26 dirty state survives unmount/remount — *real mount (`m2_semantics`) + overlay unit test*
- [x] 27 dirty state survives an injected daemon crash — *real mount (`crash_injection`: SIGKILL the serve daemon, recover, no acknowledged write lost)*
- [ ] 28 FSMonitor survives restart or safely requests full invalidation *(journal built; wiring pending)*
- [x] 29 no command requires `git lazy-mount git --` — all flows use stock git directly
- [x] 30 no ordinary workflow requires custom add/commit/switch/push — proven across `m3_git`/`m4_m5`/`git_more`

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
- [~] clean `git status`: **repeat** status fetches 0 blobs (measured, `status_hydration`); the *first* status is eager (12/12 files) until the §12.2 FSMonitor-valid bootstrap lands
- [x] `cat path`: ≤ the one required blob + its attr/filter metadata — *CI*
- [x] 100 concurrent reads of one missing file → 1 retrieval — *real mount (`m2_semantics`, single-flight)*
- [x] `O_TRUNC` open: no old-blob fetch — *real mount (`m2_semantics`, atomic_o_trunc)*
- [~] 4 KiB writes to a large file: no full rewrite (4 MiB proven `m2_semantics`); 1 GiB no-GiB-alloc pending
- [x] clean rename of unmaterialized file: 0 blob fetches — *real mount (`m4_m5`/unit)*
- [x] `git log`/`branch`/`tag`/`status`: no working-blob hydration — *real mount (inspection)*

## Process (this goal)

- Linux-only focus; runs **fully in Linux CI** through a real `/dev/fuse` mount.
- Every commit self-reviewed (reviewer teammate on substantial diffs).
- Differential tests vs a conventional checkout for every workflow (§40.1).
- The compatibility report (§3, §40.3) is generated from test results.
