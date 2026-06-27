# Real-world benchmarks

How the numbers in the project [README](../README.md#performance-in-real-world)
were measured, with full agent transcripts.

## What is measured

Each repo is set up two ways and given the **same** real `claude` (Sonnet) prompt:

1. a shallow `git clone --depth 1`, then the prompt;
2. `git lazy-mount`, then the prompt.

The prompt asks the agent to find where some piece of code lives (the questions in
the README), add a one-line clarifying comment at that site, and `git commit` +
`git push` a branch — all of which, in the lazy case, run **through the mount**.
Code search goes through [`sgrep`](../crates/sgrep), which queries a cloud index and
reads **zero** local files, so the agent only materializes the file it edits.

Run cold in a privileged Ubuntu 24.04 container with `/dev/fuse`, on the current
upstream repos; the agent's commit is pushed to a fork.

## Results

| repo | files | `git clone --depth 1` | `git lazy-mount` | file content fetched |
|---|---|---|---|---|
| facebook/react | 7,243 | 53 MB | 18 MB → 36 MB | 3 MB |
| microsoft/vscode | 16,018 | 278 MB | 97 MB → 159 MB | 1 MB |
| microsoft/TypeScript | 81,369 | 429 MB | 23 MB → 82 MB | 9 MB |

`git lazy-mount` is the on-disk workspace **right after mounting → after the agent
finished**. It keeps the **full commit history** (the clone is shallow) yet starts
smaller than even a shallow clone. Of the lazy footprint, only **1–9 MB** is actual
file *content* (sgrep answers the search; the agent reads just the one file it
edits) — the rest is the `tree:0` commit history, plus the trees Git faults while
building and pushing the commit (the mount→after-task growth). A normal full
`git clone` would be **1.08 / 1.63 / 3.4 GB** — what lazy-mount avoids while keeping
that history.

All six runs completed end to end, including the lazy runs on the 16k-file vscode
and the 81k-file TypeScript trees — each agent searched, edited, committed, and
**pushed** a branch through the mount.

### End-to-end time

Like `git lazy-mount`, a full `git clone` keeps the whole history — but it must
download it all before the task can start, where the mount is ready in seconds.
That head start, plus a metadata path that serves git's working-tree walk from
cache rather than re-reading the tree per directory, lets the mount finish first
**end to end** (set up, then run the prompt):

| repo | full `git clone` + task | `git lazy-mount` + task |
|---|---|---|
| react | 58 + 57 = **115 s** | 4 + 76 = **80 s** |
| vscode | 168 + 189 = **357 s** | 8 + 169 = **177 s** |
| TypeScript | 170 + 559 = **729 s** | 3 + 645 = **648 s** |

The task portion alone is still somewhat higher on the mount — file reads cross
FUSE and Git faults the trees it needs to build and push the commit on demand — but
the instant mount more than offsets it, on top of the order-of-magnitude smaller
disk. The per-directory walk itself is cheap (the mount serves it from a tree
cache), so the cost no longer scales with the file count the way a per-file walk
would.

## Transcripts

Full `claude` session transcripts (every tool call + result, with `[+Ns]` time
offsets from the start):

- [`transcripts/react-full.md`](transcripts/react-full.md) · [`react-lazy.md`](transcripts/react-lazy.md)
- [`transcripts/vscode-full.md`](transcripts/vscode-full.md) · [`vscode-lazy.md`](transcripts/vscode-lazy.md)
- [`transcripts/typescript-full.md`](transcripts/typescript-full.md) · [`typescript-lazy.md`](transcripts/typescript-lazy.md)

## Reproduce

```bash
cd benchmarks
docker build -t glm-bench .                 # ubuntu + rust + git-lazy-mount + sgrep + claude (non-root)
printf 'ANTHROPIC_API_KEY=...\nGH_TOKEN=...\n' > .benchenv && chmod 600 .benchenv
./run.sh react  facebook/react  <your-fork>/react  facebook/react  main  'where does `useState` resolve its initial state?'
```

See [`bench_repo.sh`](bench_repo.sh) for the per-repo driver and [`run.sh`](run.sh)
for launching one. The image runs as a non-root user so `claude` can run headlessly
with a scoped tool allow-list; FUSE works via `--device /dev/fuse --cap-add SYS_ADMIN`.
