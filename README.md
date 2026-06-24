# `git-lazy-mount`

**Lazily mount a git repo without cloning it. Files materialize as they are read or edited.**

```bash
git lazy-mount https://github.com/example/huge-repo ~/huge-repo
```

After it returns, **your ordinary `git` and tools just work**

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

Since files are not materialized, running a Grep command like `git grep` or `rg` would force a lot of files to be materialized on disk. This will defeat the point, and AI agents run the Grep tool a lot.

In a viable implementation, the `Grep` tool of the AI agent should be customized to use remote search tools — something like [Sourcegraph](https://sourcegraph.com). Naturally, this customization should take into account the locally modified files.

## Platform Support

**Linux only**: because almost all microVMs are Linux-based.

The whole stack (a transparent kernel-mounted working tree) is built on Linux FUSE (libfuse3, `/dev/fuse`).

### Windows and macOS

Windows and macOS are not supported. The design notes and feasibility studies are kept under [`docs/design/future-platforms/`](docs/design/future-platforms/) if we pick them up later.


## Install / build

```bash
# Linux. Needs libfuse3 + the system git (>= 2.36).
cargo build --release -p glm-cli --features fuse   # produces `git-lazy-mount`
```

## Docs

* [How it works and internals](docs/design/architecture.md) — the architecture overview, with per-area docs (the [worktree projection](docs/design/worktree-model.md), [FUSE semantics](docs/design/fuse-semantics.md), [object fetching](docs/design/object-fetching.md)) in [`docs/design/`](docs/design/).
* [Design specification](design.md) — the authoritative spec the implementation is built and tested against.
* [Limitations](docs/design/limitations.md) and the [git compatibility report](docs/design/compatibility.md).