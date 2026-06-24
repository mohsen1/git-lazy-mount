# Architecture Decision Records

Short records of the load-bearing decisions in git-lazy-mount, each grounded in
the code as it exists today. Format: Title, Status, Context, Decision,
Consequences. See [../design/architecture.md](../design/architecture.md) for the system
overview.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-git-cli-as-source-of-truth.md) | The installed `git` binary is the source of truth | Accepted |
| [0004](0004-object-id-format-agnostic.md) | Object IDs are format-agnostic | Accepted |
| [0005](0005-overlay-base-ref-for-renames.md) | Overlay base-refs make clean renames fetch-free | Accepted |
| [0007](0007-attr-source-for-bare-store-filtering.md) | Faithful filtering passes `--attr-source` | Accepted |

Records about subsystems that the transparent design replaced (the old
object-provider, operation log, and FSKit/IPC daemon) were retired with that
code; the decisions above are the ones the current Linux design still rests on.
