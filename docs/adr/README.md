# Architecture Decision Records

Short records of the load-bearing decisions in git-lazy-mount, each grounded in
the code as it exists today. Format: Title, Status, Context, Decision,
Consequences. See [../architecture.md](../architecture.md) for the system
overview.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-git-cli-as-source-of-truth.md) | The installed `git` binary is the source of truth | Accepted |
| [0002](0002-synchronous-object-provider-with-coalescing.md) | Synchronous object provider with thread-based coalescing | Accepted |
| [0003](0003-append-only-operation-log.md) | Append-only operation log with an atomic `CURRENT` pointer | Accepted |
| [0004](0004-object-id-format-agnostic.md) | Object IDs are format-agnostic | Accepted |
| [0005](0005-overlay-base-ref-for-renames.md) | Overlay base-refs make clean renames fetch-free | Accepted |
| [0006](0006-provider-is-residency-authority.md) | The provider is the object-residency authority | Accepted |
| [0007](0007-attr-source-for-bare-store-filtering.md) | Faithful filtering passes `--attr-source` | Accepted |
| [0008](0008-fskit-extension-delegates-to-daemon-over-ipc.md) | The FSKit extension delegates filesystem callbacks to the daemon over IPC | Proposed |

Where an ADR deviates from the spec, it says so explicitly (see 0002).
