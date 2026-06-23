# Security model

*(spec §46)*

git-lazy-mount mounts repositories you may not control. The governing
assumption is therefore explicit:

> **Repository content and repository-LOCAL configuration are UNTRUSTED by
> default.** A `.gitattributes`, a `filter` driver, a hook, a `.gitmodules`
> URL, or an in-tree `.git/config` fragment is attacker-controlled data, not a
> license to run code. Nothing the repository ships executes until you say so.

Object *integrity* (that a blob hashes to its oid, that the history is
connected) is delegated to Git, which is the source of truth for the object
database (see [architecture.md](architecture.md)). This document covers what
git-lazy-mount adds on top: the trust boundary, non-interactive I/O, path
safety, and a per-threat status table.

## Trust model

Trust is a per-repository, persistent, user-granted capability. It is stored
by `glm-filters::TrustStore` as a set of credential-free repository ids
(`RepoId`) in a JSON file under the per-user config root (e.g.
`<config>/trust.json`), written atomically (temp file, `fsync`, rename).

The API is deliberately small:

| Method | Effect |
| --- | --- |
| `is_trusted(repo)` | Is this repository trusted? (default: no) |
| `grant(repo)` | Add the repository to the trusted set; persist. |
| `revoke(repo)` | Remove it; persist. |
| `list()` | Enumerate trusted repository ids. |

Exposed on the CLI as:

```bash
git lazy-mount trust show     # is the current mount's repo trusted?
git lazy-mount trust grant    # trust it (enables external filters/hooks/etc.)
git lazy-mount trust revoke   # withdraw trust
```

Trust keys on the **credential-free repository identity** (`glm-platform`
`repo_id`), so the decision follows the repository, not a particular URL or
clone path, and is shared across all mounts of the same repository.

### What trust gates

Without trust for a repository, git-lazy-mount must **not**:

* run external clean/smudge **filter drivers** (see
  [filters-and-lfs.md](filters-and-lfs.md) — `decide()` returns `Refuse` for a
  configured external filter under `Faithful` when `trusted == false`);
* run **hooks** of any kind;
* initialize **networked submodules** (see [submodules.md](submodules.md));
* run any other **repository-provided command**.

Granting trust enables exactly these repository-provided behaviors and nothing
broader.

### Hydration never executes hooks

Hydration (faulting objects in, materializing content) is pure data movement.
It is **never** a hook trigger: there is no checkout, no `post-checkout`/
`post-merge`/`post-index-change` invocation, no filter-process spawn outside
the trusted-and-applicable path. A file becoming locally resident has no
side effect a repository can hook.

## Non-interactive filesystem callbacks

Every `git` subprocess in `glm-git-store` is built by one helper that pins a
non-interactive, hook-free, lock-light environment:

```text
GIT_TERMINAL_PROMPT=0     # a missing credential fails; it never blocks on a prompt
GIT_OPTIONAL_LOCKS=0      # read-shaped commands take no opportunistic locks
GIT_NO_LAZY_FETCH=1       # set whenever the policy forbids network (cache-only paths)
```

`GIT_TERMINAL_PROMPT=0` is the load-bearing one for the filesystem: a read that
reaches Git for a missing object can **never** pop a credential dialog or hang
an application waiting on `stdin`. Combined with the provider's
[`FetchPolicy`](../crates/core/src/fetch.rs) (`CacheOnly`/`MustNotFetch` for
filesystem callbacks — see [architecture.md](architecture.md) and
[performance.md](performance.md)), a filesystem read is either served from
local objects or fails with a clean, typed error
(`offline_missing_object`/`remote_missing_object`); it never escalates to the
network or to an interactive prompt on its own.

## No credential persistence

git-lazy-mount stores **no credentials or tokens** in workspace metadata. Git
owns the network protocol and authentication; git-lazy-mount never copies a
token, password, or secret into the overlay, stage, operation log, registry, or
trust store. Structured errors carry a `recommended_action` and redacted
breadcrumbs but, by contract, the one-line `summary` "must not contain secrets"
(see `glm-core::Error`, spec §47). The control protocol has a
`CredentialRefresh` operation (`glm-ipc`) that asks Git to re-authenticate a
repository — it refreshes auth, it does not stash it here.

## Path safety

* **Strict validation.** A repository path is a `RepoPath`
  ([crates/core/src/path.rs](../crates/core/src/path.rs)), validated at
  construction from raw bytes. It **rejects**: a NUL byte (`ContainsNul`), an
  absolute path / leading `/` (`Absolute`), an empty component such as `a//b`
  (`EmptyComponent`), and a `.` or `..` traversal component (`Traversal`).
  Paths are raw bytes, not implicitly UTF-8 (spec §17/§30): identity is
  `as_bytes()`, human display is explicitly lossy, and neither is used as the
  other.
* **No shell command construction.** `git` is always invoked via `Command` with
  separate `argv` arguments — **never** through a shell. There is no string
  interpolation into a command line, so a path or ref cannot inject shell
  metacharacters. (Non-UTF-8 paths cannot be passed to Git's `--path=` filter
  plumbing and are rejected with `invalid_repository_path` rather than
  smuggled; see [filters-and-lfs.md](filters-and-lfs.md).)
* **Reversible escaping for logs/JSON.** `RepoPath::escape`/`unescape`
  percent-encode non-printable and non-ASCII bytes so a hostile filename cannot
  inject newlines, terminal escapes, or `%`-confusion into a log line or JSON
  field, and the original bytes still round-trip exactly. Lossy Unicode is
  never an identity key.

## Threat status

Honest status for the threats enumerated in spec §46. "Mitigated" means
addressed by code that exists today; "Partial" means real but incomplete;
"Future" means designed but not yet built.

| Threat | Status | Notes |
| --- | --- | --- |
| **Path traversal** (`..`, absolute, empty component) | **Mitigated** | `RepoPath::from_bytes` rejects `Traversal`/`Absolute`/`EmptyComponent`/`ContainsNul`; `join` re-validates each component. |
| **Symlink races / TOCTOU on overlay writes** | **Partial** | No-follow / `*at` "beneath" semantics for overlay writes are designed but **not** implemented; the overlay currently stores materialized content keyed by a **path-hash**, not by walking attacker-controllable directory entries, which sidesteps in-tree symlink redirection for overlay storage. A hardened no-follow write path is future work. |
| **Case-folding / Unicode-confusable collisions** | **Partial** | `PlatformPathCollision` (`EINVAL`) is a defined, typed outcome and paths keep their exact bytes as identity (no lossy folding). Systematic detection/quarantine of confusable or case-folding collisions on case-insensitive volumes is **not** yet implemented. |
| **Decompression / resource exhaustion (zip-bomb-style blobs)** | **Partial** | A typed `ResourceLimit` (`ENOSPC`) category exists for surfacing limit breaches, but configurable size/inflate budgets are not yet enforced in the read path. Decompression itself is performed by Git. |
| **Huge directories** | **Mitigated (by design shape)** | `readdir` is `O(entries in that one directory)` and never reads whole-tree state (see [performance.md](performance.md)); there is no quadratic directory walk. Per-directory entry-count caps are not yet a configurable limit. |
| **Malicious filter output** | **Mitigated** | External filters do not run at all without trust (`decide()` ⇒ `Refuse`); `DenyExternal` refuses them even when trusted. When they do run, they run through Git's own filter plumbing and their bytes are content-addressed in the filtered-content cache (key includes filter identity), so a change in behavior cannot silently reuse a stale entry. A filter failure surfaces as a typed `filter_failure` error, not a crash. |
| **Stale mount registration** | **Partial** | The mount registry is crash-safe (atomic temp-file + `fsync` + rename) and `MountState` models `Recovering`/`Failed`. Active reaping of stale/dead registrations (and the liveness signal it would need) lands with the socketed daemon. |
| **PID reuse** | **Future** | There is no PID-based liveness check today (the CLI is per-process; no resident daemon owns a PID). Robust liveness without PID-reuse ambiguity is part of the daemon design. |
| **Control-socket impersonation** | **Future** | `glm-ipc` defines a **versioned** request/response protocol, but the socket transport and its peer authentication/authorization are not implemented. The socketed daemon and control-socket auth are explicitly future (spec §39). |

### Honesty notes

* The **socketed daemon and control-socket authentication are future.** Today
  the CLI drives the engine in-process via the `Controller`; there is no
  long-lived listener to authenticate against.
* **Symlink no-follow / "beneath" APIs for overlay writes are designed but not
  implemented.** The overlay's current path-hash addressing avoids
  *resolving* in-tree symlinks for its own storage, but this is not the same as
  a fully hardened TOCTOU-safe write path, and we do not claim it is.

We do not claim hardening that is not demonstrated; partial and future items
above are labeled as such on purpose.
