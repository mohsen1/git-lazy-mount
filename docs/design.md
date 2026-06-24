# `git-lazy-mount`: a transparent, stock-Git, lazily hydrated working tree in Rust

This is the design of `git-lazy-mount` — the authoritative specification the
implementation is built and tested against. It targets **Linux only** (FUSE);
macOS (FSKit) and Windows (ProjFS) are out of scope but possible, with notes
under [`future-platforms/`](future-platforms/).

The design is deliberately a clean one, not an incremental refactor: it does not
carry a custom stage, custom branch state, a commit-adoption bridge, or a
headless-first architecture. The real `.git/index` is the only stage and stock
Git owns refs, HEAD, commits, merges, and conflict stages.

The executable is named:

```text
git-lazy-mount
```

so Git exposes it as:

```bash
git lazy-mount
```

---

# 1. Product contract

The primary command is:

```bash
git lazy-mount https://github.com/example/huge-repo ~/huge-repo
```

This command replaces the initial `git clone` for this working copy.

It must not return successfully until:

1. the partial Git repository exists;
2. the virtual filesystem is mounted;
3. the projected root is readable;
4. the synthetic `.git` entry resolves correctly;
5. stock Git recognizes the repository;
6. the FSMonitor integration is live;
7. a basic `git status` health check succeeds.

After the command returns, the following must use the user’s ordinary Git executable without wrappers, shell aliases, environment activation, or `git lazy-mount` workflow verbs:

```bash
cd ~/huge-repo

ls
cat README.md
$EDITOR src/main.rs

git status
git diff
git add src/main.rs
git add -p
git commit -m "Change behavior"
git commit --amend
git log --oneline
git branch
git switch feature
git checkout other-branch
git merge topic
git rebase main
git stash
git fetch
git pull
git push

cargo build
make
rg 'pattern'
```

Running an unrelated command such as:

```bash
git clone https://github.com/example/another-repo ../another-repo
```

from inside the mounted directory must behave like ordinary Git.

After mounting, `git lazy-mount` commands are limited to lifecycle, diagnostics, cache management, and explicit prefetching:

```bash
git lazy-mount unmount ~/huge-repo
git lazy-mount list
git lazy-mount doctor ~/huge-repo
git lazy-mount stats ~/huge-repo
git lazy-mount trace ~/huge-repo
git lazy-mount prefetch ~/huge-repo src tests
git lazy-mount dehydrate ~/huge-repo src
git lazy-mount recover ~/huge-repo
```

Do not provide alternative workflow commands such as:

```text
git lazy-mount add
git lazy-mount commit
git lazy-mount branch
git lazy-mount switch
git lazy-mount push
git lazy-mount git --
```

Those commands indicate that transparency has failed.

---

# 2. Definition of “stock Git works”

The primary compatibility target is an unmodified, upstream Git executable discovered from the user’s `PATH`.

Using these integration points is allowed:

```text
a standard .git gitfile
normal Git configuration
normal Git hooks
core.fsmonitor
the real Git index
normal refs and reflogs
normal partial-clone configuration
normal alternates, in a later optimization phase
```

These are not allowed as the default product design:

```text
shadowing or replacing the git executable
shell aliases for built-in Git commands
a required command wrapper
LD_PRELOAD interception
process-specific filesystem lies
a disposable Git repository per command
importing or “adopting” commits after Git exits
a second staging database
a second authoritative branch database
a second implementation of Git commit semantics
```

The filesystem must expose the same namespace and contents to Git, editors, builds, and ordinary applications. Do not return different filesystem results based on the caller’s process name.

---

# 3. Two separate success dimensions

For every Git command, report two independent classifications:

## 3.1 Compatibility

```text
correct
partially correct
unsupported
```

Correct means its:

```text
exit status
stdout and stderr behavior
HEAD
refs
reflogs
index
pseudorefs
working-tree contents
conflicts
hooks
resulting commits
```

match a normal checkout.

## 3.2 Laziness and performance

```text
fully lazy
bounded hydration
potentially eager
```

A command is not “fully supported at scale” merely because it produces the right result after fetching every changed blob.

For example, unmodified Git may attempt to materialize every path changed by a branch switch. Measure that behavior rather than hiding it.

Maintain a generated compatibility report containing both dimensions.

---

# 4. Lessons from the previous implementation

Turn every item below into a regression test or architectural invariant.

## 4.1 Do not report a mount before mounting

The prior controller created the mountpoint directory and immediately stored a `Mounted` registry state.

The new lifecycle must distinguish:

```text
creating
cloning
initializing-git
building-index
starting-daemon
mounting
validating
mounted
quiescing
unmounting
recovering
failed
```

Only enter `mounted` after a kernel mount and Git health checks succeed.

## 4.2 Do not maintain two stages

The previous implementation had:

```text
a custom staged-delta database
a generated throwaway .git/index
a commit-adoption step
```

The new implementation has one stage:

```text
the real $GIT_DIR/index
```

`git add`, `git add -p`, `git reset`, `git restore --staged`, merges, and conflict stages must operate on that index directly through stock Git.

## 4.3 Do not maintain two ref models

The previous implementation used private workspace refs, attached-branch leases, and later adopted commits created elsewhere.

The new implementation lets Git own:

```text
HEAD
refs/heads/*
refs/remotes/*
refs/tags/*
ORIG_HEAD
FETCH_HEAD
MERGE_HEAD
REBASE_HEAD
CHERRY_PICK_HEAD
REVERT_HEAD
BISECT_*
sequencer state
rebase state
reflogs
```

Plain `git commit`, `git amend`, `git rebase`, and `git push` must update normal Git state directly.

## 4.4 Do not use skip-worktree as an unproven universal trick

Do not mark every index entry skip-worktree merely because the filesystem is virtual.

A full projected tree and sparse-checkout semantics are not equivalent.

Any dynamic skip-worktree or sparse-index design must pass dedicated feasibility tests for:

```text
status
add
add -p
rm
mv
checkout
switch
restore
reset
merge
rebase
stash
clean
files materialized outside the sparse set
files visible virtually but not materialized physically
```

The correctness baseline must work without skip-worktree.

Do not use `assume-unchanged` as a substitute.

## 4.5 Directory listing must not hydrate file contents

The prior path:

```text
readdir
  -> attr_for
    -> file_size
      -> read and filter blob
```

can hydrate every file in a listed directory.

The new invariant is:

```text
readdir returns names, inode IDs, and d_type only
```

Plain directory enumeration must not request exact file sizes or read blob contents.

`readdirplus` must remain disabled unless measurements prove it is beneficial and does not cause mass hydration.

## 4.6 Do not buffer entire files per callback

Do not expose APIs such as:

```rust
fn raw_blob(...) -> Result<Vec<u8>>;
fn read_file(...) -> Result<Vec<u8>>;
fn write_at(...) {
    read_entire_file();
    modify_buffer();
    rewrite_entire_file();
}
```

All content paths must be streaming or file-descriptor based.

Memory usage must not be proportional to blob size.

## 4.7 Implement real file handles

Returning `fh = 0` for every open is not sufficient.

Implement durable handle tables for:

```text
open
create
read
write
flush
fsync
release
opendir
readdir
releasedir
```

Open-unlink, rename while open, append, truncation, writable mappings, and concurrent access depend on real handles.

## 4.8 Do not spawn one native thread per callback

Use a bounded executor with:

```text
backpressure
priorities
cancellation
separate network and local-I/O limits
deadlock detection
metrics
```

## 4.9 Empty directories must be real workspace state

An empty directory cannot exist only as an inode-table entry.

Persist untracked empty directories in the overlay namespace. They must survive:

```text
lookup
readdir
unmount
remount
daemon restart
rename
rmdir checks
git clean -d
```

Git will simply omit empty directories from commits.

## 4.10 The change journal must be durable

A process-local `Mutex<Vec<ChangeRecord>>` is not a sufficient FSMonitor implementation.

Tokens must survive daemon restarts, or the daemon must return a full-invalidation response.

## 4.11 Do not overbuild platform scaffolding before a Linux vertical slice

Complete transparent Linux behavior before implementing FSKit or ProjFS.

A crate that compiles on another platform is not platform support.

---

# 5. Foundational architecture

Implement five primary components:

```text
normal per-workspace Git repository
virtual working-tree model
durable writable overlay
long-running mount daemon
Linux FUSE projection
```

The initial production implementation must not depend on a shared bare store.

---

# 6. Use a normal per-workspace Git repository first

For each mount, create a normal partial-clone repository with its administrative Git directory stored outside the mount.

Conceptual layout:

```text
~/.local/share/git-lazy-mount/
  workspaces/
    <workspace-id>/
      git/                  # real native Git administrative directory
      state.sqlite
      overlay/
      filtered-cache/
      journal/
      mount.json
      logs/
```

The mounted working tree is:

```text
~/huge-repo/
```

At its root, project a synthetic read-only regular file:

```text
.git
```

whose content is:

```text
gitdir: /absolute/path/to/workspaces/<workspace-id>/git
```

The administrative Git directory must be on a normal native filesystem, not inside FUSE.

This lets stock Git use its normal:

```text
index.lock
packed-refs
ref locks
reflogs
config locks
sequencer state
merge state
rebase state
commit message files
hook execution
atomic renames
```

Do not synthesize the contents of the entire `.git` directory through FUSE.

Protect the synthetic `.git` entry from:

```text
unlink
rename
replacement
chmod
write
directory creation beneath it
```

A Git tree entry that conflicts with Git’s protected `.git` namespace must fail safely.

## 6.1 Recommended initialization experiment

Evaluate a workflow equivalent to:

```bash
git clone \
  --filter=blob:none \
  --no-checkout \
  --separate-git-dir=<native-gitdir> \
  <url> \
  <temporary-anchor>
```

Then configure the administrative repository to use the mountpoint as its worktree.

Do not depend on a temporary physical checkout after initialization.

Use Git itself for:

```text
protocol negotiation
authentication
partial clone
object format
reference format
remote setup
branch tracking
fetch
push
signing
```

Do not assume SHA-1, 40-character IDs, or the files ref backend.

---

# 7. Git is authoritative for repository state

The following are owned exclusively by Git:

```text
HEAD and branch attachment
refs and reflogs
remote-tracking refs
the index and conflict stages
commit creation
amend and history rewriting
merge and rebase state
stash refs
bisect state
tags
push and fetch configuration
```

Do not mirror these into an independent workspace state machine.

The daemon may cache parsed versions with a checksum or generation, but caches are disposable and must be rebuilt from the real Git directory.

---

# 8. The custom state represents only the virtual working tree

A virtual filesystem still needs to remember what bytes the working tree contains independently of HEAD and the index.

This is not a second staging system.

Represent the working tree as:

```text
baseline projected tree
+
overlay namespace and content
+
tombstones
+
synthetic control entries
```

Initial state:

```text
baseline = initial checked-out commit tree
overlay = empty
```

Path resolution order:

```text
1. synthetic entries such as root .git
2. overlay file, directory, or symlink
3. overlay tombstone
4. overlay rename/subtree mapping
5. baseline Git tree entry
6. absent
```

The baseline answers:

> What would this unmaterialized path contain in the logical working tree?

It does not answer:

```text
what is staged
what HEAD is
what branch is checked out
```

Those answers come from Git.

## 8.1 Why a baseline is necessary

Index-only operations can change the index without changing working-tree bytes.

Examples:

```bash
git reset --mixed <commit>
git restore --staged path
git rm --cached path
git update-index --cacheinfo ...
```

If the projection always used the current index as its content source, these commands would incorrectly change or delete working-tree files.

The baseline and overlay preserve working-tree state while the real index changes independently.

## 8.2 Baseline advancement

Initially favor correctness over aggressive compaction.

Advance or replace the baseline only after a command is known to have updated the working tree, such as a successful checkout-like operation.

Keep overlay entries for local modifications that must survive the baseline change.

A later compaction pass may dematerialize an overlay entry only after proving that:

```text
its contents match the new projected baseline
its Git-relevant mode matches
no writable handle remains open
no pending fsync exists
no concurrent rename references it
```

Never dematerialize based only on timestamps.

---

# 9. Long-running daemon is part of the MVP

The CLI must not instantiate a new independent workspace engine for every invocation.

A per-user daemon must own:

```text
FUSE sessions
open file handles
overlay namespace transactions
the changed-path journal
object fetch scheduling
long-lived Git object readers
filtered-content cache
inode allocation
mount registration
credential failure state
metrics
recovery
```

The command:

```bash
git lazy-mount <url> <path>
```

may start the daemon when absent, but must wait for the mount to become ready.

Use a secured Unix-domain socket on Linux.

The daemon protocol must be versioned and authenticate the local user through socket ownership and peer credentials.

Do not require root privileges.

---

# 10. Exact mount startup sequence

Implement startup as an idempotent transaction.

## 10.1 Preflight

Validate:

```text
Git executable and minimum supported version
FUSE availability
mountpoint ownership and emptiness
mountpoint not nested beneath another managed mount
data-directory permissions
remote URL
credential availability
partial-clone filter support
filesystem case behavior
symlink support
available disk space
stale registry entries
stale native Git locks
```

Do not prompt for credentials from a FUSE callback.

Authentication may be interactive during the initial mount command.

## 10.2 Create the Git repository

Create a normal per-workspace partial clone using:

```text
--filter=blob:none
--no-checkout
full history by default
normal origin configuration
normal local branch and upstream
```

Support explicit options:

```bash
git lazy-mount <url> <path> --branch main
git lazy-mount <url> <path> --depth 1
git lazy-mount <url> <path> --filter blob:none
git lazy-mount <url> <path> --allow-full-object-clone
```

If the remote rejects the requested filter, fail with an actionable message unless the user explicitly allowed a full object clone.

A full object clone must still not imply a full checkout.

## 10.3 Initialize working-tree state

Record:

```text
baseline commit/tree
empty overlay
initial namespace generation
initial FSMonitor token
```

## 10.4 Initialize the real index

The correctness-first implementation may create a full index from the initial tree.

This may be O(number of tracked paths), but must fetch no blob contents.

Measure and report:

```text
index creation time
index size
peak memory
tree objects read
blob objects fetched
```

Do not market mount creation as O(1) while constructing a full index.

## 10.5 Configure Git integration

Configure at least:

```text
core.worktree
core.bare=false
core.fsmonitor=<absolute hook/client path>
core.fsmonitorHookVersion=2
core.untrackedCache=true, after capability testing
index.version=4, after compatibility testing
core.fileMode based on mount behavior
core.symlinks based on mount behavior
core.ignoreCase based on mount behavior
```

Preserve the user’s ordinary global configuration.

Do not overwrite user identity, signing, editor, pager, aliases, credential helpers, or remote policies.

## 10.6 Start and validate the mount

Start FUSE, then verify:

```bash
test "$(cat <mountpoint>/.git)" = "gitdir: <expected-gitdir>"
git -C <mountpoint> rev-parse --is-inside-work-tree
git -C <mountpoint> rev-parse --show-toplevel
git -C <mountpoint> symbolic-ref --short HEAD
git -C <mountpoint> status --porcelain=v2
```

Also perform:

```text
root readdir
lookup of one tracked path
read of a small tracked file
write/create/delete in a disposable untracked test path
FSMonitor query
```

Remove the disposable path.

Only then mark the mount active and return success.

---

# 11. Index and scalability feasibility gate

The index is the central scale question. Resolve it with measurements before building broad features.

Implement and compare these profiles.

## 11.1 Profile A: full index plus FSMonitor

Characteristics:

```text
normal index semantics
maximum stock-Git compatibility
O(number of tracked paths) index construction
possibly O(number of index entries) index parsing
no working-tree scan after FSMonitor bootstrap
```

This is the correctness baseline.

Evaluate:

```text
index format v4
split index
untracked cache
FSMonitor-valid bits
feature.manyFiles
preload-index behavior
initial status behavior
subsequent clean status behavior
```

Find a way to bootstrap FSMonitor-valid state without reading every working-tree blob.

## 11.2 Profile B: dynamic skip-worktree

Only investigate this after Profile A works.

Potential model:

```text
unmaterialized clean paths have skip-worktree
materialized or modified paths do not
the virtual filesystem still exposes all paths
```

This is experimental.

Prove behavior for every required Git command. In particular, prove that Git does not:

```text
clear bits across the entire tree
reject ordinary git add
remove projected paths unexpectedly
write skipped files during conflicts
misreport deleted or modified paths
corrupt sparse-index state
```

Do not require users to pass `git add --sparse`.

## 11.3 Profile C: sparse index

Measure whether a sparse index can represent unmaterialized subtrees while the virtual filesystem still exposes them.

Do not assume normal sparse-checkout rules fit this product.

## 11.4 Profile D: minimal Git integration

Stock Git may be correct but eager for operations such as branch switching, because it obtains and writes every changed blob itself.

If Profiles A–C cannot meet the large-repository performance requirements, design a minimal, upstreamable Git extension rather than adding a command wrapper.

A possible extension would let Git ask a virtual-working-tree provider to:

```text
declare paths virtual and clean
update a projected baseline without writing file bytes
materialize only paths requiring conflict resolution or local edits
report changed paths
invalidate projected paths
```

Requirements for such an extension:

```text
plain `git` remains the user command
the repository advertises the extension explicitly
unaware Git versions refuse safely if required
the patch is isolated and documented
the correctness profile still works with upstream Git
```

Do not silently ship a private Git fork while claiming upstream Git compatibility.

---

# 12. FSMonitor v2 integration

Implement a durable FSMonitor v2 endpoint.

Git invokes it with:

```text
protocol version
opaque previous token
```

It returns:

```text
new opaque token
NUL
zero or more NUL-separated relative paths
```

The response must be inclusive. False positives are acceptable; false negatives are not.

Record:

```text
file creation
content modification
truncation
chmod affecting Git mode
unlink
old and new names for rename
directory creation
directory deletion
directory rename
symlink creation or replacement
```

When the daemon cannot prove continuity, return the full-invalidation path:

```text
/
```

Scenarios requiring full invalidation include:

```text
journal loss
database rollback
token from another workspace
token from a future generation
journal compaction beyond requested token
unreconciled daemon crash
backend event overflow
external overlay modification
```

## 12.1 Durability

Store tokens and events in a durable append log or SQLite WAL.

A token must identify:

```text
workspace
journal epoch
monotonic sequence
projection generation
```

## 12.2 Initial index state

Develop a measured bootstrap process that marks initial index entries FSMonitor-valid without hashing their working-tree contents.

The first clean status and all subsequent clean statuses must fetch zero blob contents.

**Implementation finding:** the *first* clean status proved fundamentally unachievable as zero-blob under `blob:none`. Git must populate the index stat (including each file's size) to skip the content check, and the size requires fetching the blob — the FSMonitor-valid bit does not override an empty-stat entry. So the first clean status faults each tracked blob once; only *subsequent* clean statuses are zero-blob. FSMonitor still delivers correct change detection and skips the redundant stat scan. (Verified with `GIT_TRACE_FSMONITOR`; recorded in the limitations doc.)

## 12.3 Untracked paths

Integrate with Git’s untracked cache.

Directory metadata must change when children are created, removed, or renamed.

Do not use one constant synthetic directory mtime forever.

## 12.4 Barrier semantics

Provide an internal barrier that waits until all FUSE operations acknowledged before a captured sequence are visible to the FSMonitor query.

The hook should remain a tiny IPC client. Heavy work belongs in the daemon.

---

# 13. Observe Git directory changes without replacing Git

Install or chain notification hooks for:

```text
post-index-change
reference-transaction
post-checkout
post-merge
post-commit
post-rewrite
post-applypatch
```

These hooks notify the daemon about:

```text
index replacement
possible skip-worktree changes
HEAD movement
ref transactions
working-tree-updating operations
history rewrites
merge completion
```

Hooks are an optimization and synchronization aid, not the only correctness mechanism.

Also watch native administrative state such as:

```text
index
index.lock
HEAD
packed-refs
refs/
logs/
MERGE_HEAD
CHERRY_PICK_HEAD
REBASE_HEAD
sequencer/
rebase-merge/
rebase-apply/
```

On daemon restart or missed events, reconcile from disk.

## 13.1 Preserve user hooks

Do not overwrite or silently disable existing hooks or `core.hooksPath`.

Build a hook multiplexer that:

1. sends a bounded notification to the daemon;
2. invokes the previously configured user hook with the original arguments, stdin, environment, and exit semantics;
3. prevents recursive invocation;
4. does not hold daemon locks while the user hook runs.

Provider notification hooks that cannot affect Git’s result must not alter the user hook’s intended exit status.

---

# 14. FUSE path and inode model

Implement a stable inode table.

Each inode record contains at least:

```text
inode number
generation
current namespace identity
entry type
link count
open-handle count
lookup reference count
deleted-but-open state
baseline or overlay source
```

Requirements:

```text
repeated lookup returns stable identity
rename preserves identity
unlink removes the name but not open handles
delete and recreate receives a new generation
forget releases kernel references safely
branch changes do not reuse stale inode generations
```

The root `.git` gitfile has a reserved stable inode.

Do not use path lookup as the only way to service an open handle. A file may no longer have a path after unlink.

---

# 15. Directory namespace

Use a persistent, parent-indexed namespace store.

Queries must support:

```text
lookup(parent, name)
children(parent)
has_children(path)
rename subtree
delete subtree
case collision detection
```

Do not implement each `readdir` by scanning every dirty path in the workspace.

A directory listing should cost approximately:

```text
Git entries directly in that directory
+
overlay changes directly in that directory
```

It must not depend on the total number of dirty paths elsewhere.

Persist:

```text
empty directories
untracked directories
tombstones
rename mappings
directory generations
```

---

# 16. Required Linux FUSE operations

Implement and test:

```text
init
destroy
lookup
forget
getattr
setattr
open
create
read
write
flush
fsync
release
opendir
readdir
releasedir
mkdir
rmdir
unlink
rename and rename2 flags
symlink
readlink
link, or a clearly documented error
access
statfs
getxattr/listxattr/setxattr/removexattr policy
fallocate policy
copy_file_range
lseek
file locking policy
```

Do not call a feature complete merely because editors can save one small file.

---

# 17. Real file-handle design

Allocate a unique handle for each successful open.

A handle records:

```text
inode and generation
open flags
access mode
append mode
source snapshot
native cache or overlay file descriptor
dirty ranges or dirty state
path at open time for diagnostics only
deleted-but-open status
```

Possible sources:

```text
baseline Git blob
filtered-content cache file
overlay native file
new anonymous overlay file
synthetic .git content
symlink target
```

## 17.1 Read-only clean open

For an unmaterialized tracked file:

1. resolve its baseline entry;
2. resolve filter context;
3. ensure its Git object is available;
4. stream or generate the working-tree representation into a verified cache file;
5. open the cache file;
6. service range reads from the file descriptor.

Do not allocate a complete in-memory byte vector.

## 17.2 First writable open

For `O_TRUNC`:

```text
create an empty overlay file
do not fetch the baseline blob
```

For a partial overwrite, append, or writable mapping:

```text
materialize the working-tree representation once
copy, reflink, or otherwise seed an overlay file
perform subsequent writes in place
```

Do not recreate and rename the complete overlay file for each 4 KiB write callback.

## 17.3 Append

Honor `O_APPEND` atomically relative to concurrent writers.

## 17.4 Open then unlink

After unlink:

```text
namespace lookup fails
existing handles remain readable and writable
storage remains until the final release
```

## 17.5 Rename while open

Existing handles continue to refer to the same file identity.

## 17.6 Flush and fsync

Implement correct distinctions among:

```text
flush
fdatasync
fsync
release
directory fsync
```

Do not claim crash durability for writes the application never fsynced beyond ordinary filesystem guarantees.

---

# 18. Bounded I/O and callback execution

Use separate bounded pools or semaphores for:

```text
fast metadata operations
native overlay I/O
Git object decompression
external filters
network fetches
background prefetch
maintenance
```

Never run network I/O while holding:

```text
inode locks
namespace write transactions
handle-table locks
index-state locks
global database transactions
```

Never spawn one thread per FUSE request.

Support cancellation when the kernel cancels a request or the requesting process exits.

---

# 19. Avoid Git/FUSE subprocess deadlocks

Git processes run inside the mounted tree and therefore may trigger FUSE callbacks.

FUSE callbacks may need Git object access.

Follow these invariants:

```text
FUSE callbacks never invoke Git porcelain
FUSE callbacks never invoke a Git command that scans the worktree
FUSE callbacks never wait for the index lock held by the requesting Git process
object readers target the native gitdir directly
dedicated fetch operations are isolated
all mount/session file descriptors are CLOEXEC
child processes do not inherit the FUSE session descriptor
```

Prefer long-lived native-gitdir object readers such as batch `cat-file` sessions.

Set `GIT_NO_LAZY_FETCH=1` on inspection subprocesses that must never recursively initiate a fetch.

Only the dedicated fetch scheduler may intentionally cause network retrieval.

---

# 20. Object provider

The object provider must expose streaming interfaces.

Conceptually:

```rust
trait ObjectProvider {
    fn tree(
        &self,
        oid: &ObjectId,
        policy: FetchPolicy,
    ) -> Result<TreeObject>;

    fn object_info(
        &self,
        oid: &ObjectId,
        policy: FetchPolicy,
    ) -> Result<ObjectInfo>;

    fn open_raw_blob(
        &self,
        oid: &ObjectId,
        policy: FetchPolicy,
    ) -> Result<Box<dyn ReadSeek + Send>>;

    fn open_worktree_file(
        &self,
        oid: &ObjectId,
        path: &RepoPath,
        context: &FilterContext,
        policy: FetchPolicy,
    ) -> Result<ContentHandle>;

    fn ensure_objects(
        &self,
        oids: &[ObjectId],
        priority: FetchPriority,
    ) -> Result<EnsureResult>;
}
```

Do not use UTF-8 strings as repository path identity.

## 20.1 Fetch scheduler

Implement:

```text
coalescing of identical object requests
short batching window for distinct objects
per-origin concurrency limits
global bandwidth limits
request priorities
cancellation
bounded retries
authentication failure state
offline mode
network circuit breaker
structured metrics
```

One hundred concurrent reads of one missing blob must cause one remote object retrieval.

Waiting callers must receive the original fetch failure, not a later generic “missing object” error.

## 20.2 Caches

Separate:

```text
Git object database
parsed tree cache
filtered working-tree content cache
optional metadata cache
LFS object cache
```

Never store filtered working-tree bytes as a Git blob unless Git’s clean filter has produced that blob.

Cache files must be:

```text
written to temporary paths
checksummed or otherwise validated
fsynced when required
atomically published
immune to partially written reuse
```

---

# 21. Metadata and file size

A Git tree entry does not universally provide the exact projected working-tree size.

The size may differ because of:

```text
CRLF conversion
working-tree-encoding
ident
smudge filters
Git LFS
path-dependent attributes
```

Therefore:

```text
readdir must never require exact size
getattr must return correct size
getattr may cause metadata-triggered hydration when unavoidable
```

Track hydration reasons separately:

```text
content read
metadata lookup
filter evaluation
prefetch
Git command
background operation
```

Use fast paths when safe:

```text
overlay native file -> stat native file
cached filtered content -> stat cache file
unfiltered locally present blob -> object size
known metadata manifest -> validated manifest size
```

Never return a fake size merely to avoid hydration.

Document that `ls` and `ls -l` may have different hydration behavior.

An optional size manifest is a later optimization, not a correctness dependency.

---

# 22. Stable synthetic metadata

For unmaterialized clean files, provide stable synthetic:

```text
inode
mode
mtime
ctime
uid
gid
size once known
```

Metadata must remain stable across repeated lookups within a projection generation.

Directory mtime/generation must change when direct children change.

Do not mark a path modified merely because a synthetic timestamp differs from a normal checkout.

Test Git’s racy-clean behavior carefully.

---

# 23. Git filters and attributes

Projected tracked files must match a normal Git checkout under the same effective configuration.

Support:

```text
text
eol
working-tree-encoding
ident
filter
binary
Git LFS
```

A filtered-content cache key must include at least:

```text
raw blob object ID
repository path bytes
baseline or attribute-source identity
relevant .gitattributes state
relevant Git configuration digest
filter implementation identity
platform EOL mode
cache format version
```

Renaming a file across attribute boundaries must invalidate the old filtered result.

Changing `.gitattributes` must invalidate affected descendants.

Do not use lossy UTF-8 conversion to invoke Git plumbing.

## 23.1 Avoid index-lock recursion

A passive filesystem read may occur while Git holds `index.lock`.

Attribute resolution and smudge filtering in that read must not need to lock or rewrite the index.

## 23.2 External filter trust

External filters execute code.

At mount time, detect whether projected reads may require an executable filter.

Provide an explicit policy:

```text
trusted
builtins-only
error-on-external
raw, explicitly non-checkout-compatible
```

Passive hydration must never unexpectedly execute an untrusted command.

Apply resource limits and timeouts to external filters.

---

# 24. Git LFS

Support explicit modes:

```text
smudge
pointer
error
```

In `smudge` mode:

```text
use installed Git LFS tooling
fetch on first content access
avoid credential prompts from a low-level callback
cache LFS content separately
report LFS hydration separately
```

In `pointer` mode, expose the pointer blob.

In `error` mode, return an actionable error.

Plain `git add`, `git commit`, and `git push` must continue to use normal Git LFS behavior.

Do not claim support for LFS locking unless tested.

---

# 25. Stock Git index behavior

The real index is authoritative.

The daemon may parse it after atomic replacement and cache:

```text
stage-0 entries
unmerged stages 1/2/3
modes
object IDs
skip-worktree bits
FSMonitor-valid bits
index checksum
split-index references
sparse directory entries
```

It must not rewrite the index merely to mirror custom workspace state.

## 25.1 Index-only updates

When Git changes the index without updating the worktree, the virtual baseline and overlay remain unchanged.

Examples:

```bash
git reset --mixed
git restore --staged
git rm --cached
```

## 25.2 Working-tree updates

When Git writes, unlinks, renames, or creates paths through FUSE, those operations change the overlay exactly as ordinary filesystem operations would.

Do not infer worktree updates solely from a changed index.

## 25.3 Conflict stages

During merge, rebase, cherry-pick, or revert:

```text
stages 1/2/3 remain in the real index
conflict-marker files exist in the overlay
MERGE_HEAD and sequencer state remain in the real gitdir
```

Do not replace this with a custom conflict database as the source of truth.

Additional structured conflict metadata may be cached for diagnostics, but it must be reconstructable.

---

# 26. Required plain-Git compatibility surface

Do not claim transparent Git compatibility until the following commands pass mounted end-to-end tests without a wrapper.

## 26.1 Repository discovery and inspection

```bash
git rev-parse --show-toplevel
git status
git status --porcelain=v2
git diff
git diff --cached
git log
git show
git ls-files
git cat-file
git branch
git tag
git remote -v
```

## 26.2 Staging and committing

```bash
git add path
git add -A
git add -u
git add -p
git reset path
git restore --staged path
git commit
git commit -a
git commit --amend
git commit --fixup
git commit -S
git rm
git rm --cached
git mv
```

## 26.3 Branch and worktree mutation

```bash
git branch new
git switch branch
git switch -c branch
git checkout branch
git checkout -- path
git restore path
git reset --soft
git reset --mixed
git reset --hard
```

## 26.4 History operations

```bash
git merge
git merge --abort
git rebase
git rebase --continue
git rebase --abort
git cherry-pick
git cherry-pick --continue
git cherry-pick --abort
git revert
git stash
git stash pop
```

## 26.5 Remote operations

```bash
git fetch
git fetch --prune
git pull
git pull --rebase
git push
git push --force-with-lease
git push --tags
```

Plain `git push` is required. Do not retain a bespoke push command merely to impose a second lease model.

Git’s refs, remote-tracking refs, reflogs, and normal push safety are authoritative.

## 26.6 Working-tree utilities

```bash
git clean -n
git clean -fd
git grep
git blame
git bisect
git mergetool
git difftool
```

## 26.7 Maintenance

Test:

```bash
git fsck
git gc
git maintenance run
git repack
git prune --dry-run
```

With the initial per-workspace object store, these should have ordinary repository semantics.

Disable automatic maintenance only when a measured incompatibility requires it, and document the reason.

## 26.8 Worktrees and submodules

Before 1.0, define and test:

```bash
git worktree add
git worktree remove
git submodule init
git submodule update
git submodule foreach
```

A plain `git worktree add` may initially create a conventional non-lazy worktree, but it must not corrupt the lazy one.

Nested lazy submodules are a later optimization.

---

# 27. Checkout, switch, and rebase performance gate

These commands are the hardest transparency test.

For branches with a large tree delta, measure:

```text
tree objects read
blob objects fetched
bytes fetched
FUSE writes
paths materialized
index entries expanded
wall time
peak memory
```

With unmodified Git and a full index, Git may fetch and write all changed files.

Do not conceal that behavior.

The implementation must choose and document one of:

```text
correct but eager branch transitions
experimentally proven dynamic virtual/sparse index behavior
minimal Git provider extension
```

A release may be stock-Git compatible while labeling branch transitions “potentially eager,” but it must not claim google3-style lazy branch switching until demonstrated.

---

# 28. Filesystem semantics required for editors and build tools

Test real behavior from:

```text
VS Code
Vim/Neovim
Emacs
JetBrains IDEs
rust-analyzer
clangd
TypeScript language server
ripgrep
Cargo
Make
Ninja
Bazel, where practical
formatters
test runners
file watchers
```

Support common editor save patterns:

```text
open existing file
write temporary sibling
fsync temporary file
rename over original
fsync parent directory
delete backup
```

Also test:

```text
truncate then write
append
partial pwrite
sparse write
write after rename
open then unlink
rename over open target
directory rename
case-only rename
read while another process writes
writable mmap
file locks
```

---

# 29. Rename semantics

Implement:

```text
file rename
directory rename
rename over existing file
rename over empty directory where legal
RENAME_NOREPLACE
RENAME_EXCHANGE, or a documented unsupported error
case-only rename
rename with open source and destination handles
```

A clean file rename should be representable as metadata referring to the same blob without fetching its contents.

A clean subtree rename should not read descendant blobs.

Changing a path may change its Git filter context; invalidate affected filtered cache entries.

---

# 30. Symlinks, hard links, and special files

## 30.1 Symlinks

On Linux, project Git symlinks as native symlinks.

Preserve:

```text
raw target bytes
broken symlinks
relative and absolute targets
```

Never follow repository symlinks for internal overlay writes.

Protect against symlink-swap races.

## 30.2 Hard links

Git does not preserve hard-link identity.

Choose and document one Linux working-tree policy:

```text
support overlay hard links until commit, then lose identity
or return a clear unsupported error
```

Do not silently copy while pretending identity was preserved.

## 30.3 Special files

Reject or explicitly make overlay-only:

```text
device nodes
sockets
FIFOs
unsupported reparse-style objects
```

---

# 31. Raw repository paths

On Linux, Git paths are byte sequences, not necessarily UTF-8.

Use a type such as:

```rust
pub struct RepoPath(Vec<u8>);
```

Requirements:

```text
no lossy UTF-8 conversion for identity
NUL-delimited Git plumbing
safe display escaping
safe JSON escaping
no shell command construction
no `rev:path` strings for arbitrary paths
no stopping attribute lookup at the first non-UTF-8 component
```

Test paths containing:

```text
invalid UTF-8
newlines
tabs
leading dash
backslash
quotes
control characters
very long components
```

Reject:

```text
NUL
absolute repository paths
.. traversal
empty non-root components
reserved internal control paths
```

---

# 32. Overlay storage and durability

Use native files for writable content and a transactional namespace database.

SQLite WAL is acceptable if used carefully.

The namespace database stores:

```text
path bytes
parent identity
entry type
content backing identifier
Git-relevant executable state
tombstone state
rename state
inode identity
generation
directory generation
open-unlinked retention
```

Do not store large file contents in SQLite.

## 32.1 Single writer

The daemon is the authoritative overlay writer.

CLI tools and hooks communicate through IPC rather than opening and independently rewriting JSON state.

Use interprocess locking for:

```text
mount ownership
database migration
recovery
daemon startup
administrative Git initialization
```

In-process mutexes alone are insufficient.

## 32.2 Recovery

On startup:

1. validate the namespace database;
2. reconcile temporary content files;
3. preserve every file containing acknowledged user writes;
4. reconcile mounted state with the kernel;
5. reconcile native gitdir state;
6. invalidate FSMonitor continuity if uncertain;
7. quarantine ambiguous files instead of deleting them.

Provide:

```bash
git lazy-mount recover <mountpoint>
git lazy-mount recover <mountpoint> --export <directory>
```

---

# 33. Optional operation journal

A filesystem recovery journal is allowed.

It must not become a second Git history.

Its purpose is limited to:

```text
overlay namespace crash recovery
mount lifecycle
FSMonitor continuity
diagnostic audit
recovery of uncommitted working files
```

Git refs and reflogs remain the history of commits and branch movement.

Do not implement Jujutsu-style operation history before stock Git transparency works.

---

# 34. Shared object cache is a later optimization

After the per-workspace repository passes all transparent workflow tests, add optional object sharing.

The safe shape is:

```text
per-workspace writable Git object directory
+
shared read-mostly object cache as an alternate
```

Never route arbitrary stock Git writes directly into one global object directory through `GIT_OBJECT_DIRECTORY`.

A shared cache must have explicit protection against pruning objects still required by a workspace.

Use one or more of:

```text
workspace leases
keep refs
append-only cache policy
reference counting
grace periods
pin manifests
safe dissociation
```

Test:

```text
workspace A branch force-updated remotely
workspace B still references old base
shared maintenance
workspace-local gc
workspace deletion
offline reads after maintenance
```

Do not enable shared cache by default until these tests pass.

Sharing is a performance optimization, not part of basic correctness.

---

# 35. Authentication and offline behavior

Initial mount may use the user’s normal credential helper interactively.

FUSE callbacks must be noninteractive.

If credentials expire:

```text
return a bounded filesystem error
record the failed object and cause
surface a daemon diagnostic
allow `git lazy-mount doctor` or normal `git fetch` to refresh credentials
retry subsequent reads
```

Offline mode:

```bash
git lazy-mount <url> <path> --offline
git lazy-mount prefetch <path> --for-offline
```

Cached content must remain readable.

Accessing missing content must return a clear offline-missing-object error.

Dirty overlay content must never depend on network access for recovery.

---

# 36. Security model

Treat repository data as untrusted.

Protect against:

```text
path traversal
symlink races
malicious Git tree names
case and normalization attacks
cache poisoning
corrupt object responses
decompression bombs
unbounded filter output
hung filters
credential leakage
control-socket impersonation
stale PID files
mountpoint substitution
unsafe repository ownership
```

Cache and workspace directories must be private to the user.

Redact:

```text
credentials in URLs
authorization headers
secret query parameters
private paths when configured
file contents
```

Passive hydration must never run Git hooks.

Hooks run only because the user invoked a Git command that normally invokes them.

---

# 37. Observability

Expose:

```bash
git lazy-mount stats <mountpoint>
git lazy-mount trace <mountpoint>
git lazy-mount trace <mountpoint> --pid <pid>
git lazy-mount doctor <mountpoint>
```

Track at least:

```text
tree lookups
directory listings
getattr calls
metadata-triggered hydrations
content-triggered hydrations
Git-command-triggered hydrations
blob objects fetched
tree objects fetched
bytes fetched
coalesced requests
fetch batches
filtered-cache hits
raw-object hits
read latency
write latency
FUSE queue depth
open handles
dirty paths
untracked paths
tombstones
overlay bytes
FSMonitor token and journal size
FSMonitor full invalidations
Git index size and format
Git index parse time
hook notification lag
reconciliation events
daemon restarts
recovery actions
```

Every hydration event should identify:

```text
path, safely escaped
object ID
reason
requesting PID when available
bytes
cache result
latency
```

---

# 38. Hydration budgets

Turn these into automated assertions.

## 38.1 Mount

A blob-none mount must fetch zero working-file blobs merely to project the tree.

A full-index correctness profile may perform O(tracked paths) metadata work, but must report it honestly.

## 38.2 Directory listing

```bash
ls <directory>
```

must:

```text
fetch zero child blobs
run zero smudge filters
perform O(direct children) namespace work
```

## 38.3 Long listing

```bash
ls -l <directory>
```

may perform metadata-triggered hydration when exact size is otherwise unavailable.

It must report those hydrations distinctly.

## 38.4 Clean status

After FSMonitor bootstrap:

```bash
git status --porcelain=v2
```

must:

```text
fetch zero blobs
run zero smudge filters
avoid statting every projected file
```

It may still parse a full index in the correctness profile; measure that separately.

**Implementation finding:** the *first* such status cannot fetch zero blobs — it faults each blob once to populate the index stat, because the FSMonitor bootstrap above cannot make the first status lazy. The zero-blob, no-full-stat-scan behavior holds for *subsequent* clean statuses.

## 38.5 One file read

```bash
cat path/to/file
```

must fetch at most the required blob and required attribute/filter metadata, with no unrelated file contents.

## 38.6 Concurrent reads

One hundred concurrent reads of one missing file must perform one underlying object retrieval.

## 38.7 Truncation

```c
open(path, O_WRONLY | O_TRUNC)
```

must not fetch the old blob.

## 38.8 Partial write

Repeated 4 KiB writes to a 1 GiB file in one open session must not read or rewrite the complete file for every callback.

There must be no allocation proportional to 1 GiB.

## 38.9 Clean rename

Renaming an unmaterialized clean file must fetch zero blob contents.

## 38.10 Git inspection

```bash
git log
git branch
git tag
git status
```

must not hydrate working-tree blobs merely because they inspect repository metadata.

---

# 39. Feasibility experiments before broad implementation

Build executable vertical slices before creating every planned crate.

## Experiment A: real mounted `.git`

Demonstrate:

```bash
git lazy-mount <local-bare-remote> <mountpoint>
git -C <mountpoint> rev-parse --show-toplevel
```

Use a synthetic `.git` gitfile backed by a native administrative directory.

## Experiment B: zero-content readdir

Create a repository with 100,000 files in one or more directories.

Prove that:

```bash
ls <mountpoint>/large-directory
```

fetches zero blobs.

## Experiment C: transparent edit and status

```bash
printf x >> <mountpoint>/tracked.txt
git -C <mountpoint> status --porcelain=v2
```

must show the correct modification with no wrapper.

## Experiment D: real staging

```bash
git -C <mountpoint> add tracked.txt
git -C <mountpoint> diff --cached
```

must use the real index.

## Experiment E: interactive staging

Run a real pseudo-terminal test of:

```bash
git add -p
```

and stage only one hunk.

## Experiment F: real commit

Run:

```bash
git commit
git commit --amend
```

Verify normal hooks, editor, refs, reflogs, commit graph, and object storage.

There must be no commit-adoption step.

## Experiment G: checkout behavior

Measure stock Git for:

```bash
git switch
git checkout
git reset --hard
git merge
git rebase
```

over a branch changing 100,000 files.

Quantify whether Git hydrates and writes every changed path.

## Experiment H: FSMonitor bootstrap

Prove the first and subsequent clean statuses do not read every working-tree file.

## Experiment I: large-file I/O

Use a multi-gigabyte blob and prove bounded memory, correct range reads, truncation without old-content fetch, and non-quadratic writes.

Do not proceed to macOS or Windows until Experiments A–I have documented results.

---

# 40. Test strategy

## 40.1 Differential tests against a normal checkout

For every supported workflow:

1. create a conventional checkout;
2. create a lazy mount at the same commit;
3. perform equivalent commands and filesystem operations;
4. compare:

   * HEAD;
   * refs and reflogs;
   * index stages;
   * status;
   * working-tree bytes;
   * file types;
   * executable bits;
   * symlinks;
   * conflict state;
   * resulting trees and commits.

## 40.2 Real mounted tests

Unit tests and mocked FUSE callbacks are insufficient.

Run real tests through `/dev/fuse` that invoke ordinary executables against the mountpoint.

Use a dedicated Linux CI runner when hosted CI does not expose FUSE.

## 40.3 Git command matrix

For each required Git command, record:

```text
correctness result
stock Git version
exit code
hydrated objects
hydrated bytes
FUSE calls
index operations
known limitations
```

Generate documentation from these test results.

## 40.4 Filesystem model tests

Use property-based and model-based testing for operation sequences:

```text
create
open
read
write
truncate
append
rename
unlink
mkdir
rmdir
symlink
chmod
fsync
close
checkout
add
reset
commit
crash
recover
```

## 40.5 Crash injection

Inject process termination after:

```text
overlay file creation
overlay write
namespace transaction commit
rename
unlink
fsync
index.lock creation
index replacement
ref transaction prepared
ref transaction committed
FSMonitor journal append
mount registry update
FUSE mount success
health check
```

Verify no acknowledged user data is silently lost.

## 40.6 Concurrency tests

Test:

```text
editor write concurrent with git status
git add concurrent with file close
git status concurrent with rename
two Git commands competing for index.lock
fetch concurrent with hydration
daemon restart during read
unmount with open handles
multiple processes appending
```

## 40.7 Path tests

Include:

```text
invalid UTF-8 on Linux
newlines
tabs
leading dash
case collisions
Unicode normalization collisions for future macOS work
Windows-reserved names for future Windows work
long paths
root .git collision attempts
```

## 40.8 Filter tests

Include:

```text
CRLF
working-tree-encoding
ident
path-dependent attributes
modified .gitattributes
external single-file filter
long-running process filter
filter failure
filter timeout
LFS pointer
LFS hydrated content
```

---

# 41. Rust workspace

Use a focused workspace:

```text
git-lazy-mount/
  Cargo.toml
  rust-toolchain.toml

  crates/
    cli/
    daemon/
    ipc/
    git-repo/
    git-hooks/
    worktree/
    namespace/
    overlay/
    object-provider/
    filtered-cache/
    fsmonitor/
    fuse/
    platform/
    testkit/

  docs/
    architecture.md
    product-contract.md
    git-state-model.md
    worktree-model.md
    index-strategy.md
    fsmonitor.md
    fuse-semantics.md
    object-fetching.md
    filters-and-lfs.md
    durability.md
    security.md
    performance.md
    compatibility.md
    limitations.md
    adr/
```

Keep unsafe and platform FFI isolated.

Recommended dependencies may include:

```text
clap
tokio or a deliberately chosen bounded runtime
fuser
rusqlite
serde
tracing
thiserror
tempfile
nix or rustix
parking_lot where justified
```

Do not let async abstractions force complete blobs into memory.

Pin the Rust toolchain and document the minimum supported Rust version.

Require:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

---

# 42. Linux-first milestones

## Milestone 0: architecture and experiments

Deliver:

```text
Experiments A–I
index strategy comparison
stock Git checkout/switch measurements
deadlock analysis
overlay file-handle design
FSMonitor protocol design
ADRs
```

## Milestone 1: transparent read-only vertical slice

Deliver:

```text
one-command partial clone
real daemon
real FUSE mount
synthetic .git gitfile
stock git rev-parse
directory listing without blob hydration
lazy file read
bounded object streaming
```

## Milestone 2: writable filesystem semantics

Deliver:

```text
real open handles
copy-on-write
create/write/truncate/append
unlink/open-unlink
rename
directories
symlinks
flush/fsync/release
durable overlay
recovery
```

## Milestone 3: stock status, staging, and commit

Deliver:

```text
durable FSMonitor v2
real index
git status
git diff
git add
git add -p
git commit
git commit -a
git commit --amend
hooks
signing
```

## Milestone 4: branch-changing workflows

Deliver:

```text
checkout
switch
restore
reset
merge
rebase
cherry-pick
revert
stash
conflicts
abort and continue flows
```

Publish measured eagerness for each command.

## Milestone 5: remote and maintenance workflows

Deliver:

```text
fetch
pull
push
force-with-lease
tags
fsck
gc
maintenance
repack
offline mode
credential recovery
```

## Milestone 6: large-repository optimization

Deliver one proven path:

```text
optimized full index
dynamic skip-worktree
sparse index
or minimal Git provider extension
```

Do not choose based on architectural preference; choose from measurements.

## Milestone 7: optional shared object cache

Add safe alternates, leases, and cache maintenance only after all earlier milestones pass without sharing.

## Milestone 8: other platforms

Implement macOS and Windows as separate projects after the Linux architecture is proven.

Do not force them through a misleading generic FUSE abstraction.

---

# 43. Linux MVP release criteria

The Linux MVP is not complete until all of these pass through a real mount:

1. `git lazy-mount <url> <path>` performs the clone, mount, and validation.

2. The command leaves no required shell environment changes.

3. `git rev-parse --show-toplevel` identifies the mountpoint.

4. A normal `.git` gitfile points to a native administrative directory.

5. Plain `ls` fetches no file blobs.

6. Reading one missing file retrieves no unrelated blobs.

7. An editor atomic save updates the overlay correctly.

8. Plain `git status` sees the edit.

9. Plain `git add` stages it in the real index.

10. Plain `git add -p` stages selected hunks.

11. Plain `git commit` creates and advances a normal branch directly.

12. Plain `git commit --amend` works.

13. Plain `git push` sends the commit to an ordinary remote.

14. Plain `git fetch` and `git pull` work.

15. Plain `git switch` is correct and its hydration behavior is measured.

16. Merge conflicts use the real index’s conflict stages.

17. Rebase abort and continue work.

18. Stash creation and restoration work.

19. `git rm --cached` preserves the working-tree file.

20. `git reset --mixed` changes the index without changing projected bytes.

21. `git reset --hard` replaces projected working state correctly.

22. Open-unlink semantics work.

23. Empty untracked directories survive remount.

24. Partial writes do not rewrite the full file per callback.

25. Multi-gigabyte files do not require multi-gigabyte allocations.

26. Dirty state survives unmount/remount.

27. Dirty state survives an injected daemon crash.

28. FSMonitor survives restart or safely requests full invalidation.

29. No command requires `git lazy-mount git --`.

30. No ordinary workflow requires a custom add, commit, switch, or push command.

---

# 44. Do not claim completion when

Do not call the implementation transparent if any of these remain true:

```text
the mount registry says mounted without a kernel mount
plain Git cannot discover the repository
a temporary gitdir is generated per command
commits must be imported after Git exits
the custom stage differs from .git/index
status only works through a wrapper
push only works through a bespoke command
ls hydrates every file in a directory
read allocates the complete blob
each write rewrites the complete file
open file handles are path lookups in disguise
open-unlink fails
empty directories vanish immediately
one FUSE callback creates one OS thread
FSMonitor state disappears silently on restart
Git paths are converted lossily to UTF-8
shared-cache maintenance can invalidate active workspaces
macOS or Windows is called supported without a real mount test
```

---

# 45. Implementation discipline

Before writing broad production code, produce:

1. a concise architecture document;
2. the two-source-of-truth analysis;
3. the exact baseline-plus-overlay model;
4. the real-index integration plan;
5. the FSMonitor durability protocol;
6. the FUSE file-handle state machine;
7. the Git/FUSE deadlock analysis;
8. the startup and recovery state machines;
9. the index scalability experiment results;
10. the plain-Git compatibility matrix;
11. the hydration-budget test harness;
12. the Linux vertical-slice implementation.

Then continue directly through the milestones.

Do not stop at design documents.

Do not preserve an old abstraction merely because tests already exist around it.

Port useful test cases, not architectural mistakes.

Prioritize, in order:

```text
stock Git correctness
user-data durability
filesystem correctness
transparent UX
measured laziness
large-repository performance
shared-cache optimization
additional platforms
```
