# Real-world benchmarks

How the numbers in the project [README](../README.md#performance-in-real-world)
were measured, with full agent transcripts.

## What is measured

For each of three repositories, the same realistic task is run **twice**:

1. **Baseline** — `git clone <fork>`, then one `claude` (Sonnet) prompt.
2. **Lazy** — `git lazy-mount <fork>`, then the *same* prompt.

The prompt is a code-navigation question (the ones in the README). The agent:

1. finds the answer — searching with [`sgrep`](../crates/sgrep) (a cloud-index
   search), **not** `grep`/`rg`/`find`, which would read every file and materialize
   the whole tree on a lazy mount;
2. makes one small real edit (a clarifying comment at the answer site);
3. `git commit`s and `git push`es a branch (`glm-bench-{full,lazy}`) to the fork.

Everything — search overlay, edit, commit, push — happens through the mount.

Run cold in a privileged Ubuntu 24.04 container with `/dev/fuse`, on GitHub forks
of the upstream repos. `claude` is given a scoped tool allow-list
(`Read Glob Edit Write Bash(git:*) Bash(sgrep:*)`, `Grep` disallowed) so it cannot
fall back to local grep.

## Results

| repo | files | `git clone` (worktree + `.git`) | `git lazy-mount` (total on disk) | content fetched | setup |
|---|---|---|---|---|---|
| facebook/react | 7,243 | **1.08 GB** (40 MB + 1.04 GB) | **44 MB** (40 MB history + 3.5 MB blobs) | 3.5 MB | mount 5.7 s vs clone 46 s |
| microsoft/vscode | 16,017 | **1.56 GB** (226 MB + ~1.3 GB) | **~95 MB** (94 MB history) | few MB | mount ~8 s vs clone 68 s |
| microsoft/TypeScript | 35,946 | **2.83 GB** (96 MB + 2.74 GB) | **49 MB** (43 MB history + 4.3 MB blobs) | 4.3 MB | mount 3.6 s vs clone 117 s |

`git lazy-mount`'s footprint is dominated by the `tree:0` commit history (all
commits, no trees/blobs up front); only a few MB of actual file *content* is
fetched, because `sgrep` answers the search and the agent reads only the files it
needs. The forks' `.git` history is what a normal `git clone` must download in
full; the mount skips it.

### Per-task time and an honest caveat

react and TypeScript completed end to end — the agent searched, edited, committed,
and **pushed** on both the full clone and the lazy mount:

| repo | full-clone task | lazy-mount task | pushed |
|---|---|---|---|
| react | 83 s | 222 s | `glm-bench-full` + `glm-bench-lazy` |
| TypeScript | 58 s | 208 s | `glm-bench-full` + `glm-bench-lazy` |

The per-task time on the lazy mount is **higher**, almost entirely because stock
Git's startup `status` (which `claude` runs for context) walks the working tree,
and on a large lazy mount that walk is slow and faults objects. On **vscode** this
dominated: the lazy agent stalled in Git's `status` and did not finish the edit
(the full-clone run finished in ~69 s and pushed `glm-bench-full`). Setting
`status.showUntrackedFiles=no` did not help, so this looks like a genuine
big-repo slow path / contention in the mount, not just the untracked walk — a good
target for the change-journal / untracked-cache work.

So the win is **disk and instant availability** (no multi-GB clone, ready in
seconds); the cost today is **per-task latency on very large trees**.

## Reproduce

```bash
cd benchmarks
docker build -t glm-bench .                 # ubuntu + rust + git-lazy-mount + sgrep + claude (non-root)
# provide ANTHROPIC_API_KEY and a GitHub push token (GH_TOKEN) via an env file:
printf 'ANTHROPIC_API_KEY=...\nGH_TOKEN=...\n' > .benchenv && chmod 600 .benchenv
./run.sh react  <your-fork>/react  <your-fork>/react  facebook/react  main  'where does `useState` resolve its initial state?'
```

See [`bench_repo.sh`](bench_repo.sh) for the per-repo driver and [`run.sh`](run.sh)
for the parallel launcher. The image runs as a non-root user so `claude` can run
headlessly with a scoped allow-list; FUSE works via `--device /dev/fuse
--cap-add SYS_ADMIN`.

## Transcripts

Full `claude` session transcripts (every tool call and result):

- [`transcripts/react-full.md`](transcripts/react-full.md) · [`react-lazy.md`](transcripts/react-lazy.md)
- [`transcripts/typescript-full.md`](transcripts/typescript-full.md) · [`typescript-lazy.md`](transcripts/typescript-lazy.md)
- [`transcripts/vscode-full.md`](transcripts/vscode-full.md) — (the vscode *lazy* run stalled in Git's startup `status`; see the caveat above)
