# git-lazy-mount

A **transactional, Git-backed virtual working copy** in Rust. It exposes a Git
repository as a lazily populated, writable working tree: you see the whole
logical tree, content is fetched only when touched, you edit through normal
filesystem APIs (or the CLI), and you create **ordinary Git commits** that push
through **ordinary Git remotes**. No custom server is required.

```text
Git object database and refs
  + transactional workspace state
  + writable copy-on-write overlay
  + virtual filesystem projection
  + Git interoperability adapter
```

> **Project status — honest summary.** This is an ambitious system (comparable
> to Microsoft VFSForGit/Scalar and Meta EdenFS). What is implemented and
> **proven by tests against real Git** today: the entire backend-independent
> engine (Milestone 1) and most of the native Git workflow (Milestone 4),
> driven through a working CLI. The kernel filesystem projection (FUSE/FSKit/
> ProjFS) is **backend logic + scaffold**: the callback logic is implemented and
> tested, but the libfuse/FSKit/ProjFS FFI adapters that perform a real kernel
> mount are not built in this environment (no libfuse) and are the next step.
> See [docs/limitations.md](docs/limitations.md) and the
> [compatibility matrix](docs/git-compatibility.md). We do **not** claim
> transparency or platform support that has not been demonstrated.

## What works today (verified by the test suite)

* **Lazy clone** with partial-clone filters (`blob:none`) — trees come down,
  blobs do not (`git-store` integration tests).
* **List & read** the tree from Git objects; reading one file fetches exactly
  that object (CLI e2e + provider tests).
* **Request coalescing**: 100 concurrent reads of one missing blob ⇒ **one**
  fetch (object-provider test).
* **Copy-on-write writes**: truncate-without-fetch, partial overwrite preserving
  untouched bytes, clean-file rename **without fetching the blob**.
* **Three-tree status** that is `O(changed paths)` and never writes objects or
  fetches blobs.
* **Stage → commit** producing an ordinary Git commit (unchanged subtrees
  reused), **push** to a normal bare remote, with **compare-and-swap** that
  detects a concurrently moved branch instead of clobbering it.
* **Faithful filtering** (CRLF/`eol`) via Git's own plumbing with the correct
  attribute source; symlinks; executable bit.
* **Crash-safe operation log** with deterministic crash injection at every
  persistence boundary.

## Install / build

```bash
cargo build --release          # MSRV 1.85; uses the system `git` (>= 2.36)
# the executable is named `git-lazy-mount`, so Git exposes it as `git lazy-mount`
```

## Usage

```bash
git lazy-mount clone https://github.com/example/huge-repo ~/work/huge-repo --branch main
cd ~/work/huge-repo

git lazy-mount ls                       # list the root from Git trees
git lazy-mount ls src/compiler          # list a nested directory
git lazy-mount cat src/compiler/checker.rs   # fetch + read one file

# edit through the projected filesystem (FUSE backend), or headlessly:
echo 'fix' | git lazy-mount debug write src/compiler/checker.rs

git lazy-mount status
git lazy-mount add src/compiler/checker.rs
git lazy-mount commit -m "Fix checker"
git lazy-mount push
```

All inspection commands accept `--json` / `--json-lines` (stable envelopes with
`schema_version`, `workspace_id`, `operation_id`, `warnings`).

## Workspace layout

```
crates/
  core/            types: object ids, RepoPath, modes, state model, errors
  platform/        per-OS data roots; credential-free repo identity
  git-store/       authoritative `git` CLI adapter (fetch, cat-file, refs, CAS)
  object-provider/ residency authority: coalescing, batching, policy, metrics
  metadata/        tree cache + exact/manifest stat policy
  overlay/         copy-on-write store with tombstones + base-refs
  stage/           persistent staged delta (third tree)
  oplog/           append-only operation log + transactions + recovery
  filters/         filter modes, trust model, cache keys
  workspace/       transactional engine: status, commit, leases, switch
  fs-common/       stable inode map + neutral attributes
  fs-fuse/         Linux FUSE backend logic (FuseOps)
  fs-fskit/        macOS FSKit backend: bridge, capability detection, APFS
                   collisions, metadata policy, coordination, recovery (FSVolume
                   adapter is on-device, see docs/platform-macos.md)
  fs-projfs/       Windows ProjFS backend scaffold
  fsmonitor/       changed-path journal + sync barrier
  ipc/             versioned daemon control protocol
  projection/      projection trait + in-memory test backend
  daemon/          mount registry + controller
  testkit/         real ephemeral Git remotes for tests
  cli/             the `git-lazy-mount` executable
docs/              architecture, state model, feasibility, ADRs, limitations
```

## Documentation

Start with [docs/architecture.md](docs/architecture.md). Key reading:
[state-model](docs/state-model.md), [operation-log](docs/operation-log.md),
[git-object-fetching](docs/git-object-fetching.md),
[metadata-limitations](docs/metadata-limitations.md),
[git-compatibility](docs/git-compatibility.md),
[filters-and-lfs](docs/filters-and-lfs.md), [security](docs/security.md),
[failure-recovery](docs/failure-recovery.md), [performance](docs/performance.md),
the platform notes, and [limitations](docs/limitations.md). Phase-0 findings are
in [docs/feasibility/](docs/feasibility/); decisions in [docs/adr/](docs/adr/).

## License

Dual-licensed under MIT or Apache-2.0.
