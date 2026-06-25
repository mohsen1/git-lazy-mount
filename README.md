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

### What about Grep? → use `sgrep`

A content search reads every file, so on a lazy mount `rg`/`git grep` materializes the **whole repo** — defeating the point, and agents grep a lot. Measured on a lazy-mounted `colinhacks/zod` (581 files), searching `ZodError`:

| | materialized | time |
|---|---|---|
| `rg` | 11.9 MB | 188 s |
| `sgrep` | **0 KiB** | **< 1 s** |

[`sgrep`](crates/sgrep) answers from a cloud index ([Sourcegraph](https://sourcegraph.com) by default — pluggable) and overlays your uncommitted edits automatically, with zero faults (it reads the mount's change journal). Build it into the VM image:

```bash
cargo build --release -p sgrep   # → target/release/sgrep, a self-contained binary
```

Then point the agent's search at it:

- **Claude Code** — `claude --disallowed-tools Grep`, plus a `CLAUDE.md` line: "search with `sgrep` instead of `rg`/`grep`".
- **Codex** — the same line in `AGENTS.md` (or put an `sgrep` wrapper ahead of `rg` on `PATH`).

Details in [`crates/sgrep`](crates/sgrep).

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

* **Using it** — [compatibility](docs/compatibility.md) (which `git` commands work, and how lazily) and [limitations](docs/limitations.md) (what's deferred, and why).
* **How it works** — the [architecture overview](docs/architecture.md), then deep-dives into the [worktree model](docs/worktree-model.md), [FUSE semantics](docs/fuse-semantics.md), and [object fetching](docs/object-fetching.md).
* **Reference** — the full [specification](docs/design.md) and the [decision records](docs/adr/).