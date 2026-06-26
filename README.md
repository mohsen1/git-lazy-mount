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

More in [`crates/sgrep`](crates/sgrep).

## Performance in real world

`git lazy-mount` fetches the full commit history and the HEAD directory tree, but **no file contents**, blobs materialize as they are read. So the mount's footprint is git history, not the working tree: smaller than even a shallow clone, and unlike a shallow clone, `git log`/`merge`/`rebase` and branch switching all work. Measured cold in a Linux container, with one real `claude` prompt per repo:

| repo | files | full working tree | `git lazy-mount` | prompt |
|---|---|---|---|---|
| facebook/react | 7,244 | 72 MB | 19 MB | "where does `useState` resolve its initial state?" |
| microsoft/vscode | 16,001 | 301 MB | 94 MB | "where is the toggle-word-wrap command registered?" |
| microsoft/TypeScript | 81,370 | 652 MB | 27 MB | "what does `createTypeChecker` return?" |

The footprint is dominated by commit history (vscode has 160k commits); TypeScript, a huge working tree with moderate history, is the dramatic case (27 MB vs 652 MB). A real task then pulls only the files it touches.

### `git status` and `git diff` are free

On a cold mount, `git status` and `git diff` fault **zero** blobs. At mount time the tool seeds git's FSMonitor index extension, so git trusts the daemon's change journal instead of reading every file to compare it; it only checks the files you actually edit. Run them as much as you like. (`ls -l` on a file you haven't read still fetches that one file to report its size, the one thing a lazy mount can't answer for free.)

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