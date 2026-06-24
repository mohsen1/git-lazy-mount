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

> **Project status — honest summary.** An ambitious system (comparable to
> Microsoft VFSForGit/Scalar and Meta EdenFS). Proven against real Git today: the
> entire backend-independent engine and the native Git workflow, driven through a
> working CLI and exercised **in CI against huge real repositories across many
> Linux distros** (see [Proven results](#proven-results-in-ci-against-real-repositories)).
>
> Kernel filesystem projection:
> * **Linux FUSE — a real kernel mount.** Built behind the `fuse` feature and
>   validated in CI by an actual loopback mount: lazy hydration, enumeration,
>   writes, rename, and delete through the kernel.
> * **macOS FSKit — fully built, Apple-blocked.** A signed Swift `FSVolume`
>   extension over the shared engine (via a Rust C-ABI bridge) that builds, signs,
>   installs, and **registers** on macOS 26 — but the on-device *mount* is blocked
>   by a confirmed **Apple OS bug** that breaks all third-party FSKit on macOS 26
>   (reproduces on Apple's own sample; [issue #19](../../issues/19)). Per spec
>   §54, macOS is **not** "supported" until a real FSKit mount succeeds.
> * **Windows ProjFS** — still scaffold.
>
> See [docs/limitations.md](docs/limitations.md) and the
> [compatibility matrix](docs/git-compatibility.md). We do **not** claim platform
> support that hasn't been demonstrated.

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

## Proven results (in CI, against real repositories)

* **A commit on an 80k-file repo in ~60 ms, fetching zero blobs.** On
  microsoft/TypeScript (**81,369** files): lazy clone (`blob:none`, no checkout)
  in **~1 s**, then edit → stage → commit in **~63 ms** with **0** objects
  fetched. VCS operations cost O(what you touched), not O(repo size).
  ([`lazy-mount-demo.yml`](.github/workflows/lazy-mount-demo.yml))
* **End-to-end across a distro × repo × filter matrix.** A 27-scenario suite
  (clone → read → edit → stage → commit → modify → delete → rename → branch →
  diff → restore → reset → hydrate → fsck → doctor) runs in CI on **7 Linux
  distro images** — Ubuntu, Fedora (40 & 41), Rocky, **Alpine (musl)**, Arch,
  openSUSE — against **TypeScript, golang/go, nodejs/node,
  kubernetes/kubernetes**, under `blob:none` and `blob:limit` filters.
  ([`e2e-matrix.yml`](.github/workflows/e2e-matrix.yml),
  [`e2e-lazy-mount.sh`](scripts/e2e-lazy-mount.sh))
* **A real Linux FUSE kernel mount.** `cargo test -p glm-fs-fuse --features fuse`
  performs an actual loopback mount and drives it with plain `std::fs` — lazy
  hydration, `readdir`, writes, rename, delete — green in the `linux-mount` CI
  job.

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
  fs-fuse/         Linux FUSE backend: a real kernel mount via libfuse behind the
                   `fuse` feature, over the shared FuseOps (CI-validated)
  fs-fskit/        macOS FSKit backend (FskitOps) + a signed Swift FSVolume
                   extension under extension/ that builds/signs/registers
                   on-device (mount blocked by an Apple OS bug, #19)
  fskit-ffi/       C-ABI bridge that the Swift FSKit extension links
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
