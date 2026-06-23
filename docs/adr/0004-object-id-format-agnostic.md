# 0004 — Object IDs are format-agnostic

**Status:** Accepted

## Context

Git is migrating from SHA-1 to SHA-256, and the on-disk/wire object name length
differs (40 vs. 64 hex chars; 20 vs. 32 raw bytes). Code that assumes "an object
id is 40 hex characters" silently breaks on a SHA-256 repository and cannot
accommodate any future format (spec §3.16).

## Decision

Model an object name as [`ObjectId`](../../crates/core/src/object_id.rs): opaque
raw bytes tagged with an `ObjectFormat`. `ObjectFormat` has explicit `Sha1` and
`Sha256` arms plus an **`Other(String)`** arm so a future format Git reports does
not require a code change merely to *parse* (only to optimize). The format is
detected from `git rev-parse --show-object-format` and never assumed. We parse and
compare ids but **never compute hashes ourselves** — Git remains authoritative for
hashing (see ADR 0001).

Parsing validates hex and, for *known* formats, the exact digest length; for
unknown formats the length is whatever the hex implied (we do not enforce a size
we do not know). Two ids of different formats are never equal even if their bytes
coincide.

## Consequences

* SHA-256 repositories work without special-casing; a novel format parses without
  edits.
* Ids are stored as raw bytes, so equality and hashing are cheap and exact;
  `to_hex()` is used only for display and for talking to `git`.
* A "null" oid is constructed per-format for compare-and-swap "create" semantics
  (the expected-old value when creating a ref).
* Tests pin the rules: SHA-1 round-trip, SHA-256 length enforcement, non-hex
  rejection, and that an unknown format skips the length check.
