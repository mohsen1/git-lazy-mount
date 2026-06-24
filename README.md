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
test). This is a ground-up rebuild specified in [`redesign.md`](redesign.md).

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

Built and **proven through real `/dev/fuse` mounts in CI** (the `redesign linux
mount` job), Linux-first. ~28 of the 30 Linux-MVP criteria (`redesign.md` §43)
and Experiments A/C/D/E/F are validated by mounted tests; every real-mount test
is green with zero ignores. Full status: [`docs/redesign/requirements-checklist.md`](docs/redesign/requirements-checklist.md),
[`docs/redesign/compatibility.md`](docs/redesign/compatibility.md),
[`docs/redesign/limitations.md`](docs/redesign/limitations.md).

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

**Not yet (tracked in `limitations.md`):** the FSMonitor first-status bootstrap
(repeat status is already 0-blob; the *first* status is eager), crash-injection /
multi-GiB / 100k-file scale stress, and **macOS / Windows** (separate projects on
the transparent stack, after Linux — `redesign.md` §42 M8). We do **not** claim
platform support that hasn't been demonstrated by a real mount.

## Install / build

```bash
# Linux (real mount): needs libfuse3 + the system git (>= 2.36).
cargo build --release -p glm-cli --features fuse   # produces `git-lazy-mount`
```

Without `--features fuse` the binary builds cross-platform but reports that mount
support was not compiled in. MSRV 1.85.

## Usage

```bash
git lazy-mount https://host/huge-repo ~/huge-repo   # clone + mount + return
cd ~/huge-repo                                       # then: plain git, no wrapper

git lazy-mount unmount ~/huge-repo                   # lifecycle
git lazy-mount doctor  ~/huge-repo [--json]          # diagnostics
```

There is deliberately **no** `git lazy-mount add/commit/switch/push/git --` —
their presence would mean transparency had failed (`redesign.md` §1).

## Workspace layout

Per mount, a native admin directory lives outside the mount:

```text
~/.local/share/git-lazy-mount/workspaces/<id>/
  git/        real partial-clone admin dir (NOT inside FUSE)
  cache/      content-addressed materialized blobs
  overlay/    durable writable working-tree changes
```

## Repository

A focused Rust workspace; the transparent stack is `git-repo` (admin clone +
index), `worktree` (baseline + overlay projection), `fuse` (the kernel mount),
and `cli` (the `git-lazy-mount` binary), over `core` / `git-store` /
`object-provider` / `fs-common`. Design docs: [`docs/redesign/`](docs/redesign/).

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace                       # backend-independent suite
cargo test -p glm-fuse -p glm-cli --features fuse   # real mount (Linux + libfuse3)
```
