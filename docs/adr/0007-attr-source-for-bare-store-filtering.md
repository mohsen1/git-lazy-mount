# 0007 — Faithful filtering passes `--attr-source`

**Status:** Accepted

## Context

git-lazy-mount applies Git's own working-tree (smudge) filters so a projected file
matches what a real `git checkout` would write — CRLF/`eol`, and any configured
clean/smudge or LFS driver (spec §25). Git resolves which filters apply from
`.gitattributes` along the path. Normally Git reads `.gitattributes` from the
working tree and `HEAD`. But git-lazy-mount has **no checked-out working tree**,
and content is filtered against a **bare shared store** whose `HEAD` need not match
a given workspace's base commit — multiple workspaces share one store at different
base revisions. Reading attributes from the store's `HEAD` would apply the **wrong**
filtering rules.

## Decision

When invoking Git's filter plumbing, pass **`--attr-source=<base-commit>`**, where
the base commit is the workspace's own base revision.
[`GitStore::smudge_blob`](../../crates/git-store/src/store.rs) (`cat-file
--filters --path=<path>`) and the corresponding clean-side `hash-object --path`
both accept an `attr_source` and emit `--attr-source=<oid>`; the workspace passes
its base commit, and `glm-object-provider::filtered_blob` threads it through. This
makes `.gitattributes` resolution deterministic and **per-workspace**, independent
of the bare store's `HEAD`.

## Consequences

* Faithful filtering is correct even though the store is bare and shared: each
  workspace filters against *its* attributes, not the store's `HEAD`.
* This behavior is exercised by `workspace::crlf_filter_applied_faithfully`: with
  `.gitattributes` forcing `*.txt text eol=crlf`, a read of an LF-stored blob
  yields CRLF, resolved via `--attr-source` from the workspace base.
* Because filtering can change byte length (CRLF), exact `stat` size is filter-/
  context-dependent — relevant to the Windows ProjFS ContentID and placeholder
  size requirements (see [../design/future-platforms/windows.md](../design/future-platforms/windows.md)).
* Under a `blob:none` clone the `.gitattributes` blobs themselves may be absent;
  the provider lets Git fault them in when the policy permits network, and fails
  with an offline error under a cache-only policy (the caller must prefetch
  attributes).
