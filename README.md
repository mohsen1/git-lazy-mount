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

Across **20 repositories** — `facebook/react` to the 179k-file LLVM tree — a full
`git clone` totals **23 GB** vs **1.3 GB of lazy mounts (18× less)**, and each
mounts in **1–22 s** (measured in Firecracker microVMs):

![Disk to work on each repo: full git clone vs git lazy-mount](benchmarks/charts/disk.svg)

Lazy mounts keep **full history**, are ready in seconds, and materialize only the
files you touch. Full data, the time chart, and agent transcripts:
[`benchmarks/`](benchmarks/).


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