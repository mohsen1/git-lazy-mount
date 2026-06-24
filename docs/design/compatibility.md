# Git compatibility report

The design asks for two independent classifications per command (§3): a
**compatibility** verdict (correct / partially correct / unsupported) and a
**laziness** verdict (fully lazy / bounded hydration / potentially eager).

This matrix is generated from the real-mount tests (Docker + the CI `design
linux mount` job). "Correct" means the command's exit status, refs, index,
working-tree bytes, and resulting commits match a normal checkout for the cases
exercised. Laziness is the *measured* fetch behavior; where it is not yet
measured it says so.

| Command | Compatibility | Laziness | Proven by |
|---------|---------------|----------|-----------|
| `rev-parse --show-toplevel` | correct | fully lazy | `m3_git`/mount |
| `ls` / readdir | correct | fully lazy (0 blobs) | `experiment_a_b_c` |
| `cat` / read | correct | bounded (1 blob, coalesced) | `experiment_a_b_c`, `m2_semantics` |
| `status` | correct | first eager (reads each file), **repeat 0-blob** (git index refresh) | `m3_git`, `status_hydration` |
| `diff` / `diff --cached` | correct | bounded | `m3_git`, `git_extra` |
| `add` / `add -A` / `add -u` | correct | bounded | `m3_git` |
| `add -p` | correct | bounded | `git_extra` |
| `commit` / `-a` | correct | bounded | `m3_git` |
| `commit --amend` | correct | bounded | `git_more` |
| `rm --cached` | correct | fully lazy (index-only) | `git_more` |
| `reset --mixed` | correct | fully lazy (index-only; no worktree change) | `git_more` |
| `reset --hard` | correct | potentially eager (writes changed files) | `m4_m5` |
| `switch` / `switch -c` / `checkout` | correct | **potentially eager** (writes every changed file) | `m4_m5` |
| `merge` (clean) | correct | potentially eager | `m4_m5` |
| `merge` (conflict) | correct (real index stages 1/2/3 + markers) | potentially eager | `git_extra` |
| `rebase` / `--abort` | correct | potentially eager | `git_extra` |
| `stash` / `pop` | correct | potentially eager | `git_more` |
| `fetch` + `merge` | correct | bounded (faults changed blobs) | `git_extra` |
| `push` | correct | n/a | `m4_m5` |
| `log` / `show` / `ls-files` | correct | fully lazy (no working blobs) | `m3_git` |

## Not yet classified

`pull --rebase`, `cherry-pick`, `revert`, `bisect`, `blame`, `grep`, `clean`,
`worktree`, `submodule`, LFS/filter paths, and the maintenance commands
(`fsck`/`gc`/`repack`/`maintenance`) are not yet exercised by a mounted test.

## The eagerness headline (§27)

Branch-changing commands (`switch`/`checkout`/`reset --hard`/`merge`/`rebase`)
are **correct but potentially eager**: unmodified Git materializes and writes
every changed path through the FUSE write path. This is the design-sanctioned
M-stage behavior (§27) — we do **not** claim google3-style lazy branch switching.
The §27 100k-file eagerness *measurement* (tree objects read, blobs fetched,
bytes, paths materialized, wall time) is tracked as future work (P3 in
[`limitations.md`](limitations.md)); clean `status` becomes 0-blob once the
FSMonitor wiring lands (P1).
