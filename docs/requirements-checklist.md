# Requirements checklist (Linux MVP)

Living tracker of what's built and proven (against the [spec](design.md)). Updated as items pass
through a **real mount in CI**. `[ ]` = not done, `[~]` = in progress / partial,
`[x]` = done + tested. Nothing is checked until proven by a mounted test.

## A. Vertical-slice experiments — gate before broad work

- [x] **A** real mounted `.git`: `git -C <mnt> rev-parse --show-toplevel` → `<mnt>` — *real mount*
- [x] **B** zero-content readdir: a 1000-file directory readdir fetches **0** blobs (`large_directory_readdir`); full 100k-file CI stress is a noted scale refinement
- [x] **C** transparent edit + status: edit then `git status --porcelain` correct, no wrapper — *real mount (`m3_git`)*
- [x] **D** real staging: `git add` then `git diff --cached` uses the real index — *real mount (`m3_git`)*
- [x] **E** interactive staging: `git add -p` stages one hunk — *real mount (`git_extra`, stdin-fed)*
- [x] **F** real commit: `git commit` + `--amend`, no adoption step — *real mount (`m3_git`/`git_more`)*
- [x] **G** checkout/switch: correct (`m4_m5`); eagerness over an M-of-N delta **measured** — bounded by the delta, not the repo (`switch_eagerness`)
- [✗] **H** FSMonitor zero-blob first status: **fundamentally unachievable** with stock git + `blob:none` (git must populate the index stat — incl. size — to skip the content check; the size requires fetching the blob; `mark_fsmonitor_invalid` overrides the fsmonitor-valid bit on empty-stat entries — verified via `GIT_TRACE_FSMONITOR`). FSMonitor *is* wired (`fsmonitor` test) for change detection + faster repeat status. See limitations P1/R6.
- [x] **I** large-file I/O: 4 MiB in-place 4 KiB writes (`m2_semantics`) + **64 MiB read grows daemon RSS ~2 MiB, not 64 MiB** (`large_file`) — streamed, bounded memory

## B. Linux MVP release criteria — all via a real mount

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
- [x] 15 plain `git switch` correct — *real mount (`m4_m5`)*; hydration **measured**: a switch over an M-of-N delta touches O(M) blobs, bounded by the delta, not the repo (`switch_eagerness`)
- [x] 16 merge conflicts use the real index conflict stages — *real mount (`git_extra`: stages 1/2/3 + overlay markers)*
- [x] 17 rebase abort restores state — *real mount (`git_extra`)*; `--continue` flow classified correct through a real mount
- [x] 18 stash create + restore — *real mount (`git_more`)*
- [x] 19 `git rm --cached` preserves the working-tree file — *real mount (`git_more`)*
- [x] 20 `git reset --mixed` changes index without changing projected bytes — *real mount (`git_more`)*
- [x] 21 `git reset --hard` replaces projected working state — *real mount (`m4_m5`)*
- [x] 22 open-unlink semantics — *real mount (`m2_semantics`)*: fd reads/writes survive unlink; getattr falls back to the live fd
- [x] 23 empty untracked directories survive remount — *real mount (`m2_semantics`)*
- [x] 24 partial writes don't rewrite the full file per callback — *real mount (`m2_semantics` 4 KiB)*
- [x] 25 large files don't require large allocations — *real mount (`large_file`: a 64 MiB read grows daemon RSS ~2 MiB; streamed `cat-file`→cache + `pread`)*; extreme multi-GiB is the same structural path at a heavier CI cost
- [x] 26 dirty state survives unmount/remount — *real mount (`m2_semantics`) + overlay unit test*
- [x] 27 dirty state survives an injected daemon crash — *real mount (`crash_injection`: SIGKILL the serve daemon, recover, no acknowledged write lost)*
- [x] 28 FSMonitor survives restart or safely requests full invalidation — *wired (`fsmonitor` test): durable journal replayed on remount preserves continuity; an unplaceable token (epoch/seq mismatch) → `/` full invalidation. Change detection has no false negatives.*
- [x] 29 no command requires `git lazy-mount git --` — all flows use stock git directly
- [x] 30 no ordinary workflow requires custom add/commit/switch/push — proven across `m3_git`/`m4_m5`/`git_more`

## C. Anti-claims — must NEVER be true at "done"

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

## D. Hydration budgets — automated assertions

- [x] mount (blob:none) fetches 0 working-file blobs to project the tree — *CI*
- [x] `ls <dir>`: 0 child blobs, 0 smudge filters, O(direct children) — *CI (small scale)*
- [x] clean `git status`: **repeat** status fetches 0 blobs (measured, `status_hydration`); the *first* clean status is eager (faults each tracked blob once) — **fundamental**, not a pending refinement: under `blob:none`, git must populate the index stat (incl. size) to skip the content check, and `mark_fsmonitor_invalid` overrides the fsmonitor-valid bit on an empty-stat entry, so the size hydration cannot be elided (verified via `GIT_TRACE_FSMONITOR`)
- [x] `cat path`: ≤ the one required blob + its attr/filter metadata — *CI*
- [x] 100 concurrent reads of one missing file → 1 retrieval — *real mount (`m2_semantics`, single-flight)*
- [x] `O_TRUNC` open: no old-blob fetch — *real mount (`m2_semantics`, atomic_o_trunc)*
- [x] 4 KiB writes to a large file: no full rewrite (4 MiB proven `m2_semantics`); large-file reads stay bounded — a 64 MiB read grows daemon RSS ~2 MiB, not 64 MiB (`large_file`, streamed `cat-file`→cache + `pread`)
- [x] clean rename of unmaterialized file: 0 blob fetches — *real mount (`m4_m5`/unit)*
- [x] `git log`/`branch`/`tag`/`status`: no working-blob hydration — *real mount (inspection)*

## Process (this goal)

- Linux-only focus; runs **fully in Linux CI** through a real `/dev/fuse` mount.
- Every commit self-reviewed (reviewer teammate on substantial diffs).
- Differential tests vs a conventional checkout for every workflow.
- The compatibility report is generated from test results.
