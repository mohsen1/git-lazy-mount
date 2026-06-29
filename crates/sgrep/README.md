# sgrep

Remote code-search grep for [git-lazy-mount](../../README.md) working trees.

A content search reads every file, so on a lazy mount `rg`/`git grep` faults
(materializes) the whole repo, which defeats the point. `sgrep` answers the
query from a cloud search index instead, so it reads zero local files for
committed content. It overlays your uncommitted edits so results match the
working tree.

## Build & install

```bash
cargo build --release -p sgrep      # → target/release/sgrep
```

It's a self-contained binary (rustls TLS, no system libs). Drop it into your
microVM image and put it on `PATH`.

## Use

```bash
sgrep ZodError                       # repo inferred from the `origin` remote
sgrep --repo colinhacks/zod ZodError
sgrep -l --file '\.ts$' 'class \w+'  # files-with-matches, file filter
sgrep -i --literal 'TODO(perf)'      # case-insensitive, literal
sgrep --count 500 ZodError           # ask for a larger result set
```

Auth/endpoint come from the environment (same as the `src` CLI): `SRC_ENDPOINT`
(default `https://sourcegraph.com`) and `SRC_ACCESS_TOKEN`. The token is optional:
public repos work without it, but set it to search private repos your account can see.

By default `sgrep` requests up to 100 remote results. That keeps interactive
agent searches cheap; pass `--count` when you need a broader result set. Plain
identifier/text patterns are sent as literal searches automatically; patterns
using regex metacharacters still run as regex. Remote results are cached for 10
minutes under `$XDG_CACHE_HOME/git-lazy-mount/sgrep` (or
`~/.cache/git-lazy-mount/sgrep`) and then overlaid with current local edits on
every invocation. Use `--no-cache` or `SGREP_CACHE_TTL_SECS=0` for a fresh remote
query.

## Uncommitted edits, automatic

`sgrep` overlays your local edits with no extra flags: locally-changed files are
grepped on disk and their stale remote hits dropped, so new files, edited lines,
and removed matches are all correct.

It finds the changed set with zero blob faults. On a git-lazy-mount mount it
reads the mount's change journal directly, instead of `git status` (which would
materialize the whole repo on a cold mount). On a normal repo it falls back to
`git status`. You can override the set if you want:

```bash
sgrep --changed src/a.ts 'pattern'        # or --changed-from edited.txt
```

## Wiring into a coding agent

- **Claude Code** uses an embedded ripgrep for its Grep tool, so disable that
  tool and have the agent search via `sgrep` (Bash or an MCP tool). A `CLAUDE.md`
  note should say "search with `sgrep --count 50` instead of `rg`/`grep`, and
  prefer `--file` filters over piping to `head`." `sgrep` handles uncommitted
  edits on its own.
- **Codex**: shadow `rg`/`grep` on `PATH` with an `sgrep` wrapper, or instruct
  it to call `sgrep`.

## Providers (pluggable)

The cloud backend is abstracted behind the `SearchProvider` trait. Built-in:

- `sourcegraph` (default): native Sourcegraph streaming-search client.
- `exec`: a zero-recompile escape hatch. Set `SGREP_EXEC_CMD` to any command and
  `sgrep` runs it with the query in `SGREP_PATTERN`/`SGREP_REPO`/… then parses
  ripgrep-style `path:line:text` stdout.

```bash
SGREP_PROVIDER=exec SGREP_EXEC_CMD='my-search "$SGREP_PATTERN" "$SGREP_REPO"' sgrep foo
```

To add a native provider: implement `SearchProvider` in
`src/providers/<name>.rs`, then add one arm to `provider::build` and one entry to
`provider::NAMES`. The CLI, overlay, and output are all provider-agnostic.
