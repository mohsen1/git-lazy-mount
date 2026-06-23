# Feasibility: partial-clone strategies

**Question.** Can partial clone serve as our object substrate — trees available
for instant listing, blobs fetched on demand — and how does it behave over the
transports we test (spec §5.2)?

## Experiment

Seed a bare remote, enable `uploadpack.allowFilter`, then in a fresh bare store:

```
git init --bare; git remote add origin file://…
git -c protocol.file.allow=always fetch --filter=blob:none --no-tags origin
```

Inspect what is local before/after touching a blob.

## Results

* **`blob:none` works over `file://`** when the server side has
  `uploadpack.allowFilter=true` and the client allows the file transport
  (`protocol.file.allow=always`). `glm-testkit` configures the former; the
  store/CLI set the latter for local URLs only.
* After the filtered fetch: **HEAD resolves, all trees are present, no file
  blobs are present.** Listing the root and nested directories needs zero blob
  fetches (verified: `git-store` and `cli` tests).
* `object_exists(blob)` is `false` after the clone; reading it cache-only fails;
  after `fetch_objects([blob])` it is present and reads correctly; **a sibling
  blob remains absent** — we fetch exactly what is asked for.
* Fetching a single missing object is done by accessing it with lazy-fetch
  enabled (`cat-file --batch-check`), which faults it into the local store.

## Behavior notes

* **Offline:** a cache-only read of a missing object returns
  `offline_missing_object` (retryable); a network-permitted read surfaces the
  underlying Git transport error, classified into `offline_missing_object` vs
  `remote_missing_object`.
* **Filter unsupported by remote:** `git lazy-mount clone` fails by default,
  explains the missing capability, and offers `--allow-full-object-clone`
  (which still does not check out files). It never silently performs a full
  object clone.
* History/tree modes (`--history=full|tip`, `--tree-fetch=eager|lazy`,
  `--history-depth`) are exposed as explicit options; `--history=tip` is
  documented as *shallow history*, not transparent lazy history.

## Decision / status

Use `blob:none` as the default substrate; keep all missing-object access behind
the provider. Implemented: filtered clone, single-object faulting, offline
classification, filter-capability fallback messaging. Not yet exercised in tests:
`tree:0` tree-lazy mode and configurable shallow depth (the options exist;
dedicated tests are future).
