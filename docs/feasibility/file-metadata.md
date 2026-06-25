# Feasibility: file metadata and the exact-size problem

**Question.** Can `getattr`/`stat` return a correct size without fetching
content?

## Finding

Often no. A Git tree entry has no size, and the size a program sees is the
filtered working-tree size, which differs from the raw blob whenever CRLF,
`working-tree-encoding`, `ident`, clean-smudge filters, LFS, or
path-dependent `.gitattributes` apply. A symlink's `lstat` size is the link
target length (the blob content).

### Can `stat` avoid content retrieval?

| Path kind                          | Avoids fetch in exact mode? |
|------------------------------------|-----------------------------|
| overlay (locally written) file     | yes (length known locally)  |
| base-ref (clean rename target)      | no (filtered size)          |
| binary blob, no filters             | only with a raw-size manifest; else fetch |
| text blob under `autocrlf`/`eol`    | no                          |
| `working-tree-encoding`             | no                          |
| external filter / LFS               | no                          |
| symlink                             | no (needs the target blob)  |

### Measured platform dependence

With the same repo (`hello\n` committed, raw 6 bytes):

* Linux default (`autocrlf=false`): projected size **6**.
* Windows default (`autocrlf=true`, Git for Windows system config): projected
  size **7** (`hello\r\n`).

This surfaced as a real CI difference. It is correct: faithful filtering
matches a checkout. Tests pin `core.autocrlf=false` for determinism.

## Decision (release gate)

* **`exact` mode** (default): return the correct size; if it requires obtaining
  and filtering content, do so and record *metadata-triggered hydration* in the
  provider metrics. **Never** return a fake zero or an approximation
  (`workspace.file_size()` enforces this).
* **`manifest-assisted` mode**: optional content-addressed manifest of raw sizes
  + transformation flags, transportable via ordinary Git objects/refs; must not
  claim a filtered size unless its cache key includes every transformation
  input. *Designed, not implemented.*
* Clean unmaterialized entries use stable **synthetic** metadata
  (`glm-fs-common::FileAttr`); a synthetic timestamp mismatch never marks a file
  dirty.

## Status

Exact mode implemented and tested (sizes, CRLF, symlink, exec bit). Manifest mode
and a raw-size cache are future. See `docs/design/limitations.md`.
