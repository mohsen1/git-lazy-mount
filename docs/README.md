# git-lazy-mount documentation

New here? Start with the [project README](../README.md) — what it is, why it
exists, and how to install it. Then pick a track below.

## Using it

- [Compatibility](compatibility.md) — which `git` commands work through the mount, and how lazily they run.
- [Limitations](limitations.md) — what's deferred or fundamentally constrained, and why.

## How it works

- [Architecture overview](architecture.md) — the moving parts, end to end.
- [Worktree model](worktree-model.md) — the read-only baseline + the durable writable overlay.
- [Git state model](git-state-model.md) — what git owns vs. what the daemon caches.
- [FUSE semantics](fuse-semantics.md) — inodes, file handles, and the required operations.
- [Object fetching](object-fetching.md) — the object provider, fetch scheduler, filters, and metadata/size.
- [Index & scalability](index-strategy.md) — the real `.git/index` and the scalability gate.
- [FSMonitor](fsmonitor.md) — the durable change journal and the `core.fsmonitor` hook.
- [Startup, deadlock & recovery](deadlock-startup-recovery.md) — the lifecycle state machines.
- [Durability & security](durability-security.md) — overlay durability, auth/offline, and the threat model.

## Working on it

- [Requirements checklist](requirements-checklist.md) — what's built and proven (the living tracker).
- [Decision records](adr/) — the load-bearing decisions and the reasoning behind them.
- [Feasibility studies](feasibility/) — the experiments that validated the approach before it was built.

## Reference

- [Specification](design.md) — the full, authoritative design the implementation is built and tested against.
- [Future platforms](future-platforms/) — notes on Windows (ProjFS) and macOS (FSKit), out of scope today but not impossible.
