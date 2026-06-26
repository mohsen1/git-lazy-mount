# `git-lazy-mount`

**Lazily mount a git repo without cloning it. Files materialize as they are read or edited.**

```bash
git lazy-mount https://github.com/example/huge-repo ~/huge-repo
```

After it returns, **your ordinary `git` and tools just work**:

```bash
cd ~/huge-repo
vim src/main.rs
git commit -am 'Some edit'
git switch -c feature
git push
```


## Why?

This is aimed at microVMs that spin up to run coding agents against a git repository. The idea is that the coding agent can start working immediately without having to wait for a full clone.

When the agent runs a test or build, only relevant files are downloaded on demand.

### `Grep` tool in AI session

Tools like `rg` and `git grep` read every file, so they pull the whole repo and undo the point of `lazy-mount`.

To mititgate this, we can route search through [`sgrep`](crates/sgrep) instead. It queries a code-search index ([Sourcegraph](https://sourcegraph.com) by default, and pluggable) and overlays your uncommitted edits, fetching nothing.

More in [`crates/sgrep`](crates/sgrep).

## Performance in real world

Measured cold in a Linux/FUSE container. For each repo: a normal `git clone` then
one real `claude` (Sonnet) prompt, versus `git lazy-mount` then the **same** prompt.
The agent finds the answer, makes a one-line edit, and **commits and pushes a branch
back to the fork** — all through the mount. Code search routes through
[`sgrep`](crates/sgrep), so the agent never greps (materializes) the whole tree.
Full session transcripts and the harness are in [`benchmarks/`](benchmarks/).

| prompt | files | `git clone` | `git lazy-mount` | ready in |
|---|---|---|---|---|
| "where does `useState` resolve its initial state?" `facebook/react` | 7,243 | 1.08 GB | 44 MB | 6 s vs 46 s |
| "where is the toggle-word-wrap command registered?" `microsoft/vscode` | 16,017 | 1.56 GB | 95 MB | 8 s vs 68 s |
| "what does `createTypeChecker` return?" `microsoft/TypeScript` | 35,946 | 2.83 GB | 49 MB | 4 s vs 117 s |

`git clone` is the full download a normal checkout needs (working tree **plus** the
`.git` history); `git lazy-mount` is the entire on-disk workspace *after* the agent
finished. Only **3–4 MB** of file *content* was actually fetched — the rest of the
lazy footprint is the `tree:0` commit history. The mount is usable in seconds,
whereas a clone first downloads the whole history.

react and TypeScript ran end to end — the agent committed its edit and pushed the
branch through the mount (see the transcripts). The honest caveat: stock Git's
startup `status` walk over a large lazy mount is currently slow, so the per-task
time on the mount is higher than on a full checkout, and the vscode agent run
stalled there; the in-progress untracked-cache work targets exactly this.


## Linux Only

**Linux only**: because almost all microVMs are Linux-based.

The whole stack (a transparent kernel-mounted working tree) is built on Linux FUSE (libfuse3, `/dev/fuse`).

### Windows and macOS

Windows and macOS are not supported. The design notes and feasibility studies are kept under [`docs/future-platforms/`](docs/future-platforms/) if we pick them up later.


## Install / build

```bash
# Linux. Needs libfuse3 + the system git (>= 2.36).
cargo build --release -p glm-cli --features fuse   # produces `git-lazy-mount`
```

## Docs

Everything is in [`docs/`](docs/):

* **Using it**: [compatibility](docs/compatibility.md) (which `git` commands work, and how lazily) and [limitations](docs/limitations.md) (what's deferred, and why).
* **How it works**: the [architecture overview](docs/architecture.md), then deep-dives into the [worktree model](docs/worktree-model.md), [FUSE semantics](docs/fuse-semantics.md), and [object fetching](docs/object-fetching.md).

## License 

MIT + Apache