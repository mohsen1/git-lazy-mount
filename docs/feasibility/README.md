# Phase 0 — feasibility experiments

This directory records the mandatory Phase-0 findings (spec §5). Each report
states the question, what was tested, the measured result, and the design
decision it drove. These are **results**, not plans — the experiments were run
against real `git` (2.43 locally, 2.54 on CI) and the findings are reflected in
the implementation and its tests.

| Report | Question | Headline finding |
|--------|----------|------------------|
| [git-object-fetching.md](git-object-fetching.md) | Can we serve reads from a partial clone without implicit fetches? | A `cat-file --batch` session with `GIT_NO_LAZY_FETCH` **fatally exits** on a missing promisor object ⇒ the provider must be the residency authority. |
| [partial-clone.md](partial-clone.md) | Do partial-clone filters work as our object substrate? | `blob:none` over `file://` works with `uploadpack.allowFilter`; trees present, blobs absent until fetched; one fetch leaves siblings absent. |
| [file-metadata.md](file-metadata.md) | Can `stat` avoid fetching content? | Often **no** — filtered size differs from raw and is platform-dependent (`autocrlf`); exact mode fetches and records it. |
| [git-compatibility.md](git-compatibility.md) | What stock-Git behavior can we rely on? | Tree subtree mode must serialize `40000` not `040000` (else `git fsck` rejects); commits/push to a bare remote interoperate. |
| [linux-fuse.md](linux-fuse.md) | Is the FUSE callback model viable? | Callback logic is implemented/tested without libfuse; real kernel mount needs the libfuse adapter + privileged runner. |
| [macos-fskit.md](../design/future-platforms/feasibility-macos-fskit.md) | FSKit viability? | Not testable in this environment; scaffold + requirements recorded. |
| [windows-projfs.md](../design/future-platforms/feasibility-windows-projfs.md) | ProjFS viability? | Distinct architecture; not testable here; `autocrlf=true` system default already surfaced via CI. |

The object-fetching and metadata findings are **release gates** (spec §5.1/§5.3)
and are enforced by the test suite.
