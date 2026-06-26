# git-lazy-mount documentation

New here? Start with the [project README](../README.md). It covers what this is, why it exists, and how to install it. Then pick a track below.

## Using it

- [Compatibility](compatibility.md): which `git` commands work through the mount, and how lazily they run.
- [Limitations](limitations.md): what's deferred or fundamentally constrained, and why.

## How it works

- [Architecture overview](architecture.md): the moving parts, end to end.
- [Worktree model](worktree-model.md): the read-only baseline plus the durable writable overlay.
- [Git state model](git-state-model.md): what stock git owns vs. what the mount synthesizes.
- [FUSE semantics](fuse-semantics.md): inodes, file handles, and the implemented operations.
- [Object fetching](object-fetching.md): materialization, single-flight coalescing, filters, and exact size/metadata.
- [Index & scalability](index-strategy.md): the real `.git/index` (`read-tree HEAD`) and scalability notes.
- [FSMonitor](fsmonitor.md): the durable change journal and the `core.fsmonitor` hook.
- [Startup & deadlock avoidance](deadlock-startup-recovery.md): the mount startup sequence and the FUSE/git deadlock invariants.
- [Durability & security](durability-security.md): overlay durability, auth/offline, and the threat model.

## Reference

- [Specification](design.md): the lean, authoritative design the implementation is built and tested against.
- [Roads not taken](future-platforms/): the project is Linux-only; the Windows ([ProjFS](future-platforms/windows.md)) and macOS ([FSKit](future-platforms/macos.md), [on-device](future-platforms/macos-fskit-ondevice.md)) backends were retired and these notes survive only as history.
