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
The agent navigates to the answer surgically (targeted `ls` + reading only the
files it needs), so it materializes a few MB rather than the whole tree. (For an
explicit "search without reading every file" tool, see [`sgrep`](../crates/sgrep);
it was not exercised in these runs.)

Run cold in a privileged Ubuntu 24.04 container with `/dev/fuse`, on the current
upstream repos.

## Results

Disk to get a ready working copy (before the agent task):

| repo | files | `git clone --depth 1` | `git lazy-mount` |
|---|---|---|---|
| facebook/react | 7,243 | 53 MB | 19 MB |
| microsoft/vscode | 16,018 | 278 MB | 99 MB |
| microsoft/TypeScript | 35,946 | 429 MB | 28 MB |

`git lazy-mount` keeps the **full commit history** (the clone is shallow) and is
ready in a few seconds. The agent task then materializes only the files it touches
— for react and TypeScript the lazy workspace grew to ~44 MB / ~49 MB after the
agent edited a file and pushed a branch (git faults a few trees to build and send
the commit).

## A bug this surfaced (now fixed)

The first vscode lazy run **wedged** in `git status`: vscode's `.gitattributes` has
a few `eol=crlf` lines, and the FSMonitor seed was *all-or-nothing*, so all 16k
files went unseeded and the first `git status` size-faulted every blob. Fixed by
seeding **per-path** (carve out only the genuinely-converted files) —
[#60](https://github.com/mohsen1/git-lazy-mount/pull/60). With that fix vscode lazy
`git status` completes instead of hanging.

## Transcripts

Full `claude` session transcripts (every tool call and result):

- [`transcripts/react-full.md`](transcripts/react-full.md) · [`react-lazy.md`](transcripts/react-lazy.md)
- [`transcripts/typescript-full.md`](transcripts/typescript-full.md) · [`typescript-lazy.md`](transcripts/typescript-lazy.md)
- [`transcripts/vscode-full.md`](transcripts/vscode-full.md)

## Reproduce

```bash
cd benchmarks
docker build -t glm-bench .                 # ubuntu + rust + git-lazy-mount + claude (non-root)
printf 'ANTHROPIC_API_KEY=...\nGH_TOKEN=...\n' > .benchenv && chmod 600 .benchenv
./run.sh react  <clone-source>  <push-fork>  <upstream>  <branch>  '<prompt>'
```

See [`bench_repo.sh`](bench_repo.sh) for the per-repo driver and [`run.sh`](run.sh)
for launching one. The image runs as a non-root user so `claude` can run headlessly
with a scoped tool allow-list; FUSE works via `--device /dev/fuse --cap-add SYS_ADMIN`.
