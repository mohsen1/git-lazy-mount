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

### What about Grep?

A `git clone` downloads the whole repo before you can start. A lazy mount starts at zero and fetches only the files a task touches. Search is the exception: `rg` and `git grep` read every file, so they pull the whole repo and undo the point. Route search through [`sgrep`](crates/sgrep) instead. It queries a code-search index ([Sourcegraph](https://sourcegraph.com) by default, and pluggable) and overlays your uncommitted edits, fetching nothing.

Here is one real task on a lazily-mounted `colinhacks/zod` (581 files, 12 MB): find the test for `z.string().email()`, add a case, run it. Bytes fetched at each step:

```
git clone, before any work    ████████████████████████  12 MB   (all 581 files)

lazy mount, then the task:
  start                       ·                           0
  sgrep "email"               ·                           0      (searched all 581)
  read the two files          ▎                           0.07 MB
  run the test                ██████                      2.8 MB  (the module it imports)
  ──────────────────────────────────────────────────
  whole task                                              2.8 MB  of 12 MB
```

The agent starts instantly, searches the repo for nothing, and the only real cost is the module the test pulls in. The docs site and the other packages are never fetched. (A real Claude Code agent does invoke `sgrep` here; that part is verified.)

Wire the agent's search to `sgrep` (`cargo build --release -p sgrep`):

- **Claude Code**: `claude --disallowed-tools Grep`, plus a `CLAUDE.md` line, "search with `sgrep`".
- **Codex**: the same line in `AGENTS.md`, or an `sgrep` wrapper ahead of `rg` on `PATH`.

More in [`crates/sgrep`](crates/sgrep).

## Platform Support

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

Everything is in [`docs/`](docs/) ([index](docs/README.md)):

* **Using it**: [compatibility](docs/compatibility.md) (which `git` commands work, and how lazily) and [limitations](docs/limitations.md) (what's deferred, and why).
* **How it works**: the [architecture overview](docs/architecture.md), then deep-dives into the [worktree model](docs/worktree-model.md), [FUSE semantics](docs/fuse-semantics.md), and [object fetching](docs/object-fetching.md).
* **Reference**: the full [specification](docs/design.md) and the [decision records](docs/adr/).