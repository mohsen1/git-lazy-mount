# sgrep

Remote code-search grep for [git-lazy-mount](../../README.md) working trees.

A content search reads every file, so on a lazy mount `rg`/`git grep` faults
(materializes) the **whole repo** — defeating the point. `sgrep` answers the
query from a cloud search index instead (so it reads zero local files for
committed content), and overlays your uncommitted edits so results match the
working tree.

## Build & install

```bash
cargo build --release -p sgrep      # → target/release/sgrep
```

It's a self-contained binary (rustls TLS, no system libs) — drop it into your
microVM image and put it on `PATH`.

## Use

```bash
sgrep ZodError                       # repo inferred from the `origin` remote
sgrep --repo colinhacks/zod ZodError
sgrep -l --file '\.ts$' 'class \w+'  # files-with-matches, file filter
sgrep -i --literal 'TODO(perf)'      # case-insensitive, literal
```

Auth/endpoint come from the environment (same as the `src` CLI): `SRC_ENDPOINT`
(default `https://sourcegraph.com`) and `SRC_ACCESS_TOKEN` (optional — public
repos work without it; set it to search private repos your account can see).

## Uncommitted edits — automatic

`sgrep` overlays your local edits with no extra flags: locally-changed files are
grepped on disk and their stale remote hits dropped, so new files, edited lines,
and removed matches are all correct.

It finds the changed set **with zero blob faults** — on a git-lazy-mount mount it
reads the mount's change journal directly, instead of `git status` (which would
materialize the whole repo on a cold mount). On a normal repo it falls back to
`git status`. You can override the set if you want:

```bash
sgrep --changed src/a.ts 'pattern'        # or --changed-from edited.txt
```

## Wiring into a coding agent

- **Claude Code** uses an embedded ripgrep for its Grep tool, so disable that
  tool and have the agent search via `sgrep` (Bash or an MCP tool). A `CLAUDE.md`
  note — "search with `sgrep` instead of `rg`/`grep`" — is enough; `sgrep` handles
  uncommitted edits on its own.
- **Codex** — shadow `rg`/`grep` on `PATH` with an `sgrep` wrapper, or instruct
  it to call `sgrep`.

## Providers (pluggable)

The cloud backend is abstracted behind the `SearchProvider` trait. Built-in:

- `sourcegraph` (default) — native Sourcegraph streaming-search client.
- `exec` — a zero-recompile escape hatch: set `SGREP_EXEC_CMD` to any command;
  `sgrep` runs it with the query in `SGREP_PATTERN`/`SGREP_REPO`/… and parses
  ripgrep-style `path:line:text` stdout.

```bash
SGREP_PROVIDER=exec SGREP_EXEC_CMD='my-search "$SGREP_PATTERN" "$SGREP_REPO"' sgrep foo
```

**Add a native provider** in three steps: implement `SearchProvider` in
`src/providers/<name>.rs`, then add one arm to `provider::build` and one entry to
`provider::NAMES`. The CLI, overlay, and output are all provider-agnostic.
