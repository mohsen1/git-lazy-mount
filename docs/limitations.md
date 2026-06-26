# Current limitations

What constrains `git-lazy-mount` today: costs that are fundamental to lazy-blob
fetching, behaviors that are by-design, and capabilities not yet supported. For
what works per command, see [`compatibility.md`](compatibility.md).

## Fundamental costs of lazy fetching

- **`ls -l` / `stat` faults a blob for its exact size.** A Git tree entry carries
  no working-tree size, and asking Git for the size of an unmaterialized blob
  fetches the whole promisor object, so stat-ing a not-yet-materialized file
  faults its blob once. This is inherent to any lazy-blob filter (including the
  default `tree:0`) and is not closeable without a server-side size manifest.
  `git status` / `git diff` do **not** pay this cost — the seeded FSMonitor
  extension lets Git skip the stat entirely (see [`fsmonitor.md`](fsmonitor.md)).

- **Branch-changing commands are eager.** `switch`, `checkout`, `reset --hard`,
  `merge`, and `rebase` write every changed path through the FUSE write path,
  faulting each changed blob. The cost is bounded by the size of the delta, not
  the size of the repo, but there is no lazy (google3-style) branch switch.

## By-design behaviors

- **Smudge-filtered files read the raw baseline blob.** A file governed by a
  *smudge* filter (`eol=crlf`, `ident`, `working-tree-encoding`, a custom
  `filter=` / LFS driver) reads through the mount as its stored bytes — LF rather
  than CRLF, an unexpanded `$Id$`, the LFS pointer text — not the bytes a real
  checkout would write. Git's *content* comparison stays clean (the clean filter
  is the inverse) and **commits remain byte-correct**; only working-tree *reads*
  of these files diverge. Applying smudge at materialize time would make
  `getattr` size depend on filter output, breaking lazy stat and
  rename-without-fetch — a correct fix needs filter-aware lazy sizing. When any
  such conversion attribute is present, the first-status FSMonitor seed is
  skipped entirely, so Git checks these files normally.

## Not supported yet

- **End-to-end Git LFS and custom `filter=` drivers.** The *clean* filter and the
  native `text` / `eol` / `ident` attributes work, but an external `filter=lfs`
  driver end-to-end is not wired (git-lfs is not exercised in CI).
- **Full submodules and nested lazy worktrees.** Submodule workflows are only
  partially validated through the mount (the submodule test is `#[ignore]`'d).
- **A shared object cache across workspaces.** Each mount keeps its own object
  store; there is no cross-workspace sharing.

## Platform

Linux only. Windows (ProjFS) and macOS (FSKit) are out of scope; the design notes
are kept under [`future-platforms/`](future-platforms/).
