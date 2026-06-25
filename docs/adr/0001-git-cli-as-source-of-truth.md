# 0001: The installed `git` binary is the source of truth

**Status:** Accepted

## Context

git-lazy-mount needs Git's full network protocol (smart HTTP/SSH), credential
handling, partial-clone/promisor logic, the configured object format, ref
transactions, working-tree filters (clean/smudge, LFS), and push. Re-implementing
any of these in Rust would be a large, security-sensitive surface that must track
upstream Git exactly. Credential helpers and filter drivers are the worst offenders.

## Decision

Treat the installed `git` binary (>= 2.36) as the authoritative engine for all of
the above. `glm-git-store` ([store.rs](../../crates/git-store/src/store.rs)) wraps
`git` subprocesses behind a typed API: `init --bare`, `fetch` (with
`--filter`), `update-ref` CAS, `push --force-with-lease`, `cat-file`
tree/blob/`--filters`, `hash-object`, `commit-tree`, and a long-lived
`cat-file --batch-command` session. Every invocation is non-interactive
(`GIT_TERMINAL_PROMPT=0`), lock-light (`GIT_OPTIONAL_LOCKS=0`), and the object
format is detected via `rev-parse --show-object-format` rather than assumed.

A pure-Rust library (e.g. `gitoxide`) **may** be added later for fast read-only
parsing (tree/blob decode) once it is proven against the same tests. But it stays
an optimization behind the existing API, never the authority for network,
credentials, or writes.

## Consequences

* Correctness for the hard parts (auth, promisor fetch, filters) is inherited
  from Git; we do not maintain our own.
* A `git` binary is a hard runtime dependency; behavior can vary slightly across
  Git versions, so subprocess output is parsed defensively (NUL delimiters where
  paths appear, classified stderr).
* Subprocess overhead is mitigated by the persistent batch session and by
  reusing unchanged subtrees on commit. A future Rust read path can remove it for
  hot reads without changing call sites.
