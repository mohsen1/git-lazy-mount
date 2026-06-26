# Git compatibility report

Which `git` commands work through the mount, and how lazily. Each command gets
two verdicts: a compatibility verdict (correct / partial / unsupported) and a
laziness verdict counting how many blobs it has to fetch (fully lazy / bounded /
potentially eager).

This matrix is hand-maintained from the real-mount integration tests (the
`Proven by` column names each one). Those tests run on every push through the CI
`linux mount (/dev/fuse)` job, which mounts a real FUSE filesystem on GitHub's
ubuntu runners. "Correct" means the command's exit status, refs, index,
working-tree bytes, and resulting commits match a normal checkout for the cases
exercised. Laziness is the *measured* fetch behavior.

| Command | Compatibility | Laziness | Proven by |
|---------|---------------|----------|-----------|
| `rev-parse --show-toplevel` | correct | fully lazy | `m3_git`/mount |
| `ls` / readdir | correct | fully lazy (0 blobs) | `experiment_a_b_c` |
| `cat` / read | correct | bounded (1 blob, coalesced) | `experiment_a_b_c`, `m2_semantics` |
| `status` | correct | **0-blob** (first and repeat; seeded fsmonitor-valid index) | `fsmonitor` (first status, seeded), `status_hydration` (repeat status) |
| `diff` / `diff --cached` | correct | bounded | `m3_git`, `git_extra` |
| `add` / `add -A` / `add -u` | correct | bounded | `m3_git` |
| `add -p` | correct | bounded | `git_extra` |
| `commit` / `-a` | correct | bounded | `m3_git` |
| `commit --amend` | correct | bounded | `git_more` |
| `rm --cached` | correct | fully lazy (index-only) | `git_more` |
| `reset --mixed` | correct | fully lazy (index-only; no worktree change) | `git_more` |
| `reset --hard` | correct | potentially eager (writes changed files) | `m4_m5_git` |
| `switch` / `switch -c` / `checkout` | correct | **potentially eager** (writes every changed file) | `m4_m5_git` |
| `merge` (clean) | correct | potentially eager | `m4_m5_git` |
| `merge` (conflict) | correct (real index stages 1/2/3 + markers) | potentially eager | `git_extra` |
| `rebase` / `--abort` | correct | potentially eager | `git_extra` |
| `stash` / `pop` | correct | potentially eager | `git_more` |
| `fetch` + `merge` | correct | bounded (faults changed blobs) | `git_extra` |
| `push` | correct | n/a | `m4_m5_git` |
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
| `.gitattributes` smudge (eol/ident/custom) | partial: raw bytes served, but commits stay correct because the clean filter is the inverse (see [limitations.md](limitations.md)) | n/a | `survey_advanced` |
| `submodule` add/status/update | partial: not validated end-to-end through the mount | n/a | `survey_advanced` (`#[ignore]`) |

In-place edits of the same byte size are detected correctly. Overlay files
report their real on-disk mtime, so git's racy-clean logic re-checks content.

## Notes

- **Correct through real mounts**: `cherry` (range), `am`/`apply`, `notes`,
  `replace`, `cherry-pick` ranges, `tag`/`describe`/`archive`, and `bisect` all
  pass through real mounts. **LFS** (an external `filter=lfs` driver) needs a
  git-lfs/server integration, and full **submodule** workflows are partial
  (test `#[ignore]`'d).

- **Eagerness**: branch-changing commands (`switch`/`checkout`/`reset --hard`/
  `merge`/`rebase`) are correct but potentially eager — stock git writes every
  changed path through the FUSE write path. Bounded by the delta, not the repo.
  See [limitations.md](limitations.md).

- **Zero-blob first status**: the first clean `git status` faults zero blobs,
  same as every repeat, because the FSMonitor index extension is pre-seeded at
  mount (paths under a smudge conversion are carved out). The mechanism is owned
  by [fsmonitor.md](fsmonitor.md); see also [limitations.md](limitations.md).
