# Feasibility: Linux FUSE

**Question.** Is the FUSE callback model viable for the projection, and what is
needed for a real kernel mount?

## Environment finding

This development/CI environment has `/dev/fuse` present but no libfuse
installed, so a real kernel mount cannot be linked or exercised here. To keep
the build and `--all-features` green on all platforms, the `fuser` FFI is **not**
wired in. The FUSE callback logic is implemented and tested independently.

## What is implemented and tested

`glm-fs-fuse::FuseOps` maps the low-level callbacks onto the workspace engine and
the stable `InodeTable`, and is unit-tested against a real lazy clone
(`lookup`, `getattr`, `readdir`, ranged `read`):

* `readdir` reads only the directory's own tree (O(entries in that dir));
* `lookup`/`getattr` return exact sizes (fetching+filtering when needed);
* `read` lazily hydrates and supports ranged reads;
* inode identity is stable: numbers never reused, rename preserves identity,
  and open-unlink survives until `forget`. This is covered by `glm-fs-common`
  tests.

## What remains for a real mount

A thin `fuser::Filesystem` adapter (behind a `fuse` feature, requiring libfuse3
and a privileged/loopback-capable runner) that:

* implements `lookup/forget/getattr/setattr/opendir/readdir/releasedir/open/
  read/write/create/flush/fsync/release/mkdir/rmdir/unlink/rename/symlink/
  readlink/access/statfs` plus xattr/lock policy;
* translates `FileAttr` into `fuser::FileAttr` and `glm_core::Error::errno()`
  into errno;
* maintains correct lookup refcounting and bounded attribute caching with
  explicit invalidation, correct negative-entry invalidation, and open-unlinked
  behavior;
* never performs network I/O under inode-map locks;
* evaluates READDIRPLUS, writeback cache, direct I/O, keep-cache, and disables
  optimizations that would cause hidden mass hydration.

The manual CI job `linux fuse backend` installs libfuse3 and runs the backend
tests; it is the place a real loopback mount test will be added with the adapter.

## Decision

The callback model is viable and the logic is proven. Real mounting is a
well-scoped FFI adapter, deliberately gated so a green default build never
implies the kernel backend was exercised.
