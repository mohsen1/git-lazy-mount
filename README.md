# git-lazy-mount

A **transparent, lazily hydrated Git working tree** in Rust. One command replaces
the initial `git clone`:

```bash
git lazy-mount https://github.com/example/huge-repo ~/huge-repo
```

After it returns, **your ordinary `git` and tools just work** — no wrapper, no
aliases, no environment, no `git lazy-mount` workflow verbs:

```bash
cd ~/huge-repo
ls; cat README.md; $EDITOR src/main.rs
git status; git add -p; git commit; git switch -c feature; git merge; git push
cargo build; rg pattern
```

You see the whole logical tree; blob content is fetched only when a file is
actually read. There is **one** stage — the real `$GIT_DIR/index` — and Git owns
all of HEAD, refs, reflogs, commits, merges, and rebases. The result is
**byte-for-byte identical to a normal checkout** (verified by a differential
test). This is a ground-up rebuild specified in [`design.md`](design.md).

## How it works

```text
git lazy-mount <url> <path>
  ├─ partial clone (--filter=blob:none --no-checkout --separate-git-dir)
  │    → a native admin gitdir outside the mount
  ├─ a FUSE mount at <path> projecting:
  │    • a synthetic, protected `.git` gitfile → the admin gitdir
  │    • the HEAD tree (baseline), lazily hydrated, + a durable writable overlay
  └─ core.worktree = <path>, so stock git discovers and operates on the mount
```

Stock `git` reads `<path>/.git`, follows it to the admin gitdir, and treats
`<path>` as its working tree — using its normal index, locks, hooks, and refs.

## Status — honest summary

Built and **proven through real `/dev/fuse` mounts in CI** (the `linux mount`
job), **Linux only**. Nearly all of the 30 Linux-MVP criteria (`design.md` §43)
and Experiments A–F are validated by mounted tests; every real-mount test is
green with zero ignores. Full status: [`docs/design/requirements-checklist.md`](docs/design/requirements-checklist.md),
[`docs/design/compatibility.md`](docs/design/compatibility.md),
[`docs/design/limitations.md`](docs/design/limitations.md).

**Proven (real mount):**

- `git lazy-mount <url> <path>` clones, mounts, validates, and returns; the
  synthetic `.git` makes `git rev-parse --show-toplevel` resolve to the mount.
- `status` / `diff` / `add` / `add -p` / `commit` / `commit --amend`,
  `switch` / `merge` (with real index conflict stages) / `rebase` (+ `--abort`),
  `fetch` / `push`, `stash`, `rm --cached`, `reset --mixed`/`--hard` — all stock
  git, all correct, **identical to a normal checkout** (`§40.1` differential test).
- Hydration budgets: `ls` fetches 0 blobs; one `cat` fetches one blob; 100
  concurrent reads of one missing blob ⇒ one fetch (single-flight); `O_TRUNC`
  fetches no old blob; a repeat clean `status` fetches 0 blobs.
- Filesystem semantics: real file handles, copy-on-write, open-then-unlink,
  rename-while-open, editor atomic save, empty-dir survives remount, durable
  overlay (dirty state survives unmount/remount). Pathological/invalid-UTF-8
  paths round-trip.

- Durability: dirty overlay state survives an **injected daemon crash** (SIGKILL
  then recover) and unmount/remount.

**Not yet (tracked in `limitations.md`):** the FSMonitor first-status bootstrap
(repeat status is already 0-blob; the *first* status is eager), and multi-GiB /
full 100k-file scale stress (moderate scale proven). We do **not** claim anything
that hasn't been demonstrated by a real mount.

## Platforms

**Linux only.** The whole point — a transparent kernel-mounted working tree —
rides on Linux FUSE (libfuse3, `/dev/fuse`). **Windows and macOS are not
supported.**

They are not *impossible*: the engine (clone, index, baseline+overlay
projection, git interop) is platform-neutral, and the projection maps onto
**FSKit** on macOS and **ProjFS** on Windows — each is a separate backend plus
the platform's path/metadata quirks, not a rewrite. We deliberately scoped those
out to ship a correct Linux tool first. The design notes and feasibility studies
are kept under [`docs/design/future-platforms/`](docs/design/future-platforms/)
if we pick them up later.

## Install / build

```bash
# Linux (real mount): needs libfuse3 + the system git (>= 2.36).
cargo build --release -p glm-cli --features fuse   # produces `git-lazy-mount`
```

Without `--features fuse` the binary still builds (handy on a non-Linux dev host)
but reports that mount support was not compiled in. MSRV 1.85.

## Usage

```bash
git lazy-mount https://host/huge-repo ~/huge-repo   # clone + mount + return
cd ~/huge-repo                                       # then: plain git, no wrapper

git lazy-mount unmount ~/huge-repo                   # lifecycle
git lazy-mount doctor  ~/huge-repo [--json]          # diagnostics
```

There is deliberately **no** `git lazy-mount add/commit/switch/push/git --` —
their presence would mean transparency had failed (`design.md` §1).

## Workspace layout

Per mount, a native admin directory lives outside the mount:

```text
~/.local/share/git-lazy-mount/workspaces/<id>/
  git/        real partial-clone admin dir (NOT inside FUSE)
  cache/      content-addressed materialized blobs
  overlay/    durable writable working-tree changes
```

## Repository

A focused 8-crate Rust workspace: `git-repo` (admin clone + index), `worktree`
(baseline + overlay projection), `fuse` (the kernel mount), and `cli` (the
`git-lazy-mount` binary), over `core` / `git-store` / `fs-common` (+ `testkit`).
Design docs: [`docs/design/`](docs/design/).

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace                       # backend-independent suite
cargo test -p glm-fuse -p glm-cli --features fuse   # real mount (Linux + libfuse3)
```
