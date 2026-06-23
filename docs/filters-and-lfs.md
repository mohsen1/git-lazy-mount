# Working-tree filters and LFS

*(spec §25, §26)*

## Filtering is Git's job

git-lazy-mount does **not** reimplement clean/smudge conversion. The byte-level
work is done by Git's own plumbing so that the clean bytes we project are, by
construction, the bytes a real `git checkout` would write:

* **smudge** (object → working tree): `git cat-file --filters --path=<p>
  --attr-source=<base-commit> <oid>`
* **clean** (working tree → object): `git hash-object --path=<p> --stdin`
  (with `-w` only when we intend to persist the object; `status` passes
  `write = false` so it never writes a dirty blob — spec §2.7)

These live in `glm-git-store::GitStore::smudge_blob` /
`hash_blob_clean`. The crate `glm-filters` does **not** transform bytes; it
decides *whether external filters may run* and computes the filtered-content
**cache key**.

### Two verified findings about a bare shared store

A repository is cloned once into a **bare** shared store (see
[architecture.md](architecture.md)). Two consequences were verified against
real Git and shape the implementation:

1. **Attributes must be resolved with `--attr-source=<base-commit>`.** In a bare
   store the store's `HEAD` need not match the workspace's base commit. Without
   `--attr-source`, Git would resolve `.gitattributes` against the wrong (or no)
   tree and could apply the wrong filter. So every smudge/clean call passes the
   workspace base commit as the attribute source. The
   `crlf_filter_applied_faithfully` workspace test asserts this: a repo storing
   LF with `*.txt text eol=crlf` in `.gitattributes` reads back as CRLF, while
   the raw blob remains LF.

2. **Under `blob:none`, the `.gitattributes` blobs themselves can be absent.**
   A partial clone fetches trees but not blobs — including the `.gitattributes`
   blobs along a path. A *filtered* read therefore may need to fault those
   attribute blobs in. The provider's `filtered_blob`
   ([crates/object-provider/src/lib.rs](../crates/object-provider/src/lib.rs))
   handles this precisely:
   * when the `FetchPolicy` permits network (`may_fetch()`), it lets Git fault
     the attribute blobs in (the smudge call is allowed to fetch);
   * under a cache-only policy it forbids the fetch, so the smudge fails with a
     **clean offline error** (`offline_missing_object`) rather than silently
     mis-filtering. The caller must prefetch attributes first.

   Corollary: a raw read of a blob does not depend on `.gitattributes`, so it
   can succeed cache-only even when a filtered read of the same blob would need
   to fetch attributes.

## Filter modes and the decision matrix

`glm-filters::FilterMode` selects the policy
([crates/filters/src/lib.rs](../crates/filters/src/lib.rs)):

| Mode | Built-in conversions (EOL/encoding/ident) | External `filter` drivers |
| --- | --- | --- |
| `Faithful` | yes | yes, **only if the repo is trusted** |
| `DenyExternal` | yes | **never** (refused even when trusted) |
| `Raw` | **no** | no |

> `Raw` projects the **raw blob bytes** and **does NOT match a normal
> checkout** (spec §25). It must be selected explicitly. Use it to inspect
> exact stored bytes, never as a stand-in for a faithful working tree.

`decide(mode, trusted, has_external_filter) -> FilterDecision` returns one of
`RunGitFilters`, `RawOnly`, or `Refuse`:

| mode | trusted | external filter? | decision |
| --- | --- | --- | --- |
| `Raw` | any | any | `RawOnly` |
| `DenyExternal` | any | yes | `Refuse` |
| `DenyExternal` | any | no | `RunGitFilters` |
| `Faithful` | yes | yes | `RunGitFilters` |
| `Faithful` | **no** | yes | `Refuse` |
| `Faithful` | any | no | `RunGitFilters` |

A `Refuse` produces an actionable `filter_failure` error pointing at
`git lazy-mount trust grant` or `--filters=raw`. Trust is the per-repository
capability described in [security.md](security.md).

## The filtered-content cache key

Filtered bytes are cached content-addressably. The key (`FilterContext::cache_key`,
a SHA-256 over a versioned, NUL-delimited encoding) includes **every input that
can change the transformation**, so a stale entry is impossible (spec §25):

| Input | Why it is in the key |
| --- | --- |
| raw blob oid (format + bytes) | different source bytes ⇒ different result |
| repo path | attributes are path-dependent |
| attr-source (e.g. base-commit tree id) | which `.gitattributes` apply |
| config digest | filter-affecting Git config changed |
| filter identity | a different driver may transform differently |
| EOL mode marker | platform/`autocrlf` EOL behavior |
| format version | tool cache-format bump invalidates everything |

Changing **any** of these yields a different key and a fresh computation; the
`cache_key_changes_with_every_input` unit test asserts that mutating the blob,
path, attr-source, config digest, or EOL mode each changes the key.

## A note on `stat` size

Under `autocrlf`/`eol` conversion the **filtered** size differs from the raw
blob size, and the exact on-disk size a checkout would report is
**platform-dependent**. git-lazy-mount does not assume the raw blob length is
the working-tree length for filtered paths. The exact-vs-manifest stat policy
and its limits are documented separately —
see [metadata-limitations.md](metadata-limitations.md).

## Git LFS

*(spec §26)*

> **LFS is NOT implemented yet.** This section documents the intended policy,
> not current behavior. A typed `lfs_failure` error category exists in
> `glm-core` as a placeholder; no LFS pointer handling, transfer, or smudge is
> wired up today.

Intended policy modes (design):

* **pointer** — project the LFS *pointer* file as-is (no LFS object fetch);
* **smudge** — resolve the pointer and fetch/serve the real LFS object
  (requires trust and network, like any external filter);
* **error** — refuse paths that require LFS resolution, with a clear
  `lfs_failure`.

Because LFS rides on the clean/smudge filter mechanism, it inherits the trust
gate above: resolving LFS content runs a repository-configured external filter
and therefore requires trust.

**Critical distinction (do not conflate residency layers):** a Git *blob* being
local says nothing about whether its *LFS object* is local. Under LFS the blob
is just a small pointer; the bytes live in a separate LFS store reached over a
separate transfer. "The blob is present" must never be read as "the content is
present" for LFS-tracked paths. This is the same orthogonality the residency
model insists on elsewhere (see [architecture.md](architecture.md)).
