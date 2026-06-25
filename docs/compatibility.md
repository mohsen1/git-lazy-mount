# Git compatibility report

Which `git` commands work through the mount, and how lazily. Each command gets
two verdicts: a compatibility verdict (correct / partial / unsupported) and a
laziness verdict counting how many blobs it has to fetch (fully lazy / bounded /
potentially eager).

This matrix is generated from the real-mount tests (Docker plus the CI `design
linux mount` job). "Correct" means the command's exit status, refs, index,
working-tree bytes, and resulting commits match a normal checkout for the cases
exercised. Laziness is the *measured* fetch behavior.

| Command | Compatibility | Laziness | Proven by |
|---------|---------------|----------|-----------|
| `rev-parse --show-toplevel` | correct | fully lazy | `m3_git`/mount |
| `ls` / readdir | correct | fully lazy (0 blobs) | `experiment_a_b_c` |
| `cat` / read | correct | bounded (1 blob, coalesced) | `experiment_a_b_c`, `m2_semantics` |
| `status` | correct | **0-blob** (first and repeat; seeded fsmonitor-valid index) | `m3_git`, `status_hydration` |
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
| `cherry-pick` / `revert` | correct | bounded (touched blobs fault in) | `survey_history` |
| `rebase --continue` | correct | bounded | `survey_history` |
| `pull --rebase` | correct | bounded (faults the fetched tip) | `survey_history` |
| `grep` (worktree / `<rev>`) | correct | potentially eager (reads searched files) | `survey_inspect` |
| `blame` | correct | bounded | `survey_inspect` |
| `bisect` (start/run/reset) | correct | per-commit checkout eager | `survey_inspect` |
| `log -p` | correct | potentially eager (diffs touched blobs) | `survey_inspect` |
| `clean -fd` | correct | fully lazy (unlink/rmdir overlay) | `survey_worktree_ops` |
| `restore` / `checkout -- <path>` | correct | bounded (one blob fault) | `survey_worktree_ops` |
| `mv <file>` / `mv <dir>` (rename) | correct | fully lazy (base-refs + subtree, no fetch) | `survey_worktree_ops` |
| `fsck` / `gc` / `repack` / `maintenance` / `prune` | correct | fully lazy (object store only) | `survey_maintenance` |
| `worktree add` (linked) | correct | potentially eager (the linked checkout hydrates) | `survey_advanced` |
| `.gitattributes` clean filter (`text=auto`) | correct | bounded | `survey_advanced` |
| `.gitattributes` smudge (eol/ident/custom) | partial: raw bytes served, but commits stay correct because the clean filter is the inverse. See limitations R7. | n/a | `survey_advanced` |
| `submodule` add/status/update | partial: not yet validated end-to-end through the mount | n/a | `survey_advanced` (`#[ignore]`) |

In-place edits of the same byte size are detected correctly. Overlay files
report their real on-disk mtime, so git's racy-clean logic re-checks content.
A constant mtime would have hidden such edits.

## Newly classified

`cherry` (range), `am`/`apply` of mailbox patches, `notes`, `replace`,
`cherry-pick` ranges, `tag`/`describe`/`archive`, and `bisect` are all now
correct through real mounts. The remaining gaps are genuinely deferred. Deep
**LFS** end-to-end (an external `filter=lfs` driver, bounded by R7) needs
a separate git-lfs/server integration, and full **submodule** workflows are
still partial (test `#[ignore]`'d).

## The eagerness headline

Branch-changing commands (`switch`/`checkout`/`reset --hard`/`merge`/`rebase`)
are correct but potentially eager. Unmodified Git materializes and writes
every changed path through the FUSE write path. This is the design-sanctioned
M-stage behavior, and we do **not** claim google3-style lazy branch switching.
The switch eagerness is now measured: it's bounded by the delta, not the
repo (`switch_eagerness`, P3).

The first clean `git status` faults zero blobs, same as every repeat status.
We pre-seed the FSMonitor index extension at mount (right after `read-tree`,
in `AdminRepo::seed_fsmonitor_valid`), marking every entry `CE_FSMONITOR_VALID`
with the journal's seq-0 token. Git's `refresh_cache_ent` then early-returns on
`CE_FSMONITOR_VALID` before any `lstat`, so no entry is stat'd and no blob is
faulted for its size. FSMonitor is wired (`core.fsmonitor` to
`git-lazy-mount-fsmonitor`) and answers "nothing changed" at the bootstrap (seq 0)
token. Paths under a checkout conversion (`filter`/`ident`/`working-tree-encoding`/
CRLF `eol`) are excluded from the seed so git checks them normally and never hides
a diff. Verified zero-fault on an 81k-file real mount (see limitations P1/R6).
