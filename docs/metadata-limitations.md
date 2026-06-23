# Metadata limitations: why exact `stat` may fetch content

A Git tree entry records a path, a mode, and an object id — **but not the blob's
size**, and certainly not the *working-tree* size after filters. Returning a
correct `stat`/`getattr` result is therefore not free.

## The exact-size problem (spec §5.1)

The size a program sees when it `stat`s a file is the size of the **filtered
working-tree representation**, which can differ from the raw blob size due to:

* CRLF conversion (`text`, `eol`, `core.autocrlf`),
* `working-tree-encoding`,
* `ident` expansion,
* clean/smudge filters and Git LFS,
* path-dependent `.gitattributes`.

So even a raw blob-size manifest is insufficient in general. For a **symlink**,
`lstat` size is the length of the link target, which is the (small) blob's
content — again not in the tree entry.

git-lazy-mount supports two metadata modes:

* **`exact`** (default) — return the correct size. If it cannot be known without
  obtaining and filtering the content, obtain it. This is recorded as
  *metadata-triggered hydration* in the provider metrics. **We never return a
  fake zero or an approximation.**
* **`manifest-assisted`** — an optional content-addressed manifest may supply raw
  object sizes and transformation flags, transportable through ordinary Git
  objects/refs (a possible future namespace `refs/lazy-mount/manifests/<commit>`,
  not required for correctness). A manifest must not claim to know the *filtered*
  size unless its cache key includes every transformation input. (Designed; the
  manifest path is not implemented yet.)

`workspace.file_size()` implements exact mode: overlay/base-ref content lengths
are known locally; for a clean base file it obtains the filtered bytes (fetching
the blob and any needed `.gitattributes` when the policy permits) and returns
their length.

Can `stat` avoid content retrieval?

| Path kind                         | `stat` avoids fetch? |
|-----------------------------------|----------------------|
| overlay (locally written) file    | yes (size is local)  |
| base-ref (clean rename target)    | no, in exact mode (filtered size) |
| normal binary blob, no filters    | only with a raw-size manifest; otherwise fetch |
| normal text blob (`autocrlf`/`eol`)| no (filtered size differs from raw) |
| `working-tree-encoding` path      | no |
| external-filter / LFS path        | no |
| symlink                           | no (needs the target blob) |

## Exact size is platform-dependent

Because filtering matches a real checkout, the exact size depends on the
effective Git configuration — including the **platform**. Git for Windows ships
`core.autocrlf=true` in system config, so `hello\n` projects as `hello\r\n`
(7 bytes) on a default Windows install but `hello\n` (6 bytes) on Linux. This is
correct behavior, not a bug. The test suite pins `core.autocrlf=false` for
determinism; see [filters-and-lfs.md](filters-and-lfs.md) and
[platform-windows.md](platform-windows.md).

## Synthetic metadata for clean entries (spec §28)

Git tracks only file type and the executable bit — not owner, group, most
permission bits, mtime/ctime, xattrs, ACLs, resource forks, ADS, or hard-link
identity. For unmaterialized clean files we expose **stable synthetic** metadata
(`glm-fs-common::FileAttr`) and must **never** mark a file dirty merely because a
synthetic timestamp differs from a physical checkout. Materialized files use
native metadata where safe. On Windows the Git executable-bit state is stored
independently of NTFS permissions.

## `readdir` is cheap; full scans are not

`readdir` needs only the current directory's tree object plus overlay namespace
changes (`O(entries in that directory)`); it never recursively enumerates
descendants. A *full* collision preflight or a full-tree search is `O(repository
entries)` and is explicitly exempted from the lazy invariants (see
[performance.md](performance.md)).
