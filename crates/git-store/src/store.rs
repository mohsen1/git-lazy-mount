//! `GitStore`: the authoritative adapter over the `git` binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use glm_core::{Error, ErrorCode, ObjectFormat, ObjectId, Result, TreeObject};

use crate::batch::BatchSession;
use crate::proc::{run, run_checked};
use crate::tree_parse;

/// Options controlling a `fetch` (spec §7).
#[derive(Clone, Debug, Default)]
pub struct FetchOptions {
    /// Partial-clone filter spec, e.g. `blob:none` or `tree:0`.
    pub filter: Option<String>,
    /// Shallow depth, if any.
    pub depth: Option<u32>,
    /// Whether to fetch tags.
    pub tags: bool,
}

/// Author/committer identity for a commit.
#[derive(Clone, Debug)]
pub struct Identity {
    /// Display name.
    pub name: String,
    /// Email address.
    pub email: String,
    /// Optional Git date string (e.g. RFC2822 or `@<unix> <tz>`).
    pub date: Option<String>,
}

/// Parameters for [`GitStore::commit_tree`].
#[derive(Clone, Debug)]
pub struct CommitParams {
    /// The tree to commit.
    pub tree: ObjectId,
    /// Parent commits (zero for a root commit).
    pub parents: Vec<ObjectId>,
    /// Commit message.
    pub message: String,
    /// Author identity, or `None` to use Git config.
    pub author: Option<Identity>,
    /// Committer identity, or `None` to use Git config.
    pub committer: Option<Identity>,
    /// Whether to GPG/SSH-sign (uses configured signing facilities).
    pub sign: bool,
}

/// A bare Git object store. Cheap to clone (just paths + format).
#[derive(Clone, Debug)]
pub struct GitStore {
    git_dir: PathBuf,
    format: ObjectFormat,
}

impl GitStore {
    /// Open an existing bare store, detecting its object format.
    pub fn open(git_dir: impl Into<PathBuf>) -> Result<GitStore> {
        let git_dir = git_dir.into();
        let format = detect_format(&git_dir)?;
        Ok(GitStore { git_dir, format })
    }

    /// Initialize a new bare store (spec §15). No physical checkout is created.
    pub fn init_bare(
        git_dir: impl Into<PathBuf>,
        format: Option<ObjectFormat>,
    ) -> Result<GitStore> {
        let git_dir = git_dir.into();
        if let Some(parent) = git_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut cmd = Command::new("git");
        cmd.arg("init").arg("--bare");
        if let Some(fmt) = &format {
            cmd.arg(format!("--object-format={}", fmt.name()));
        }
        cmd.arg(&git_dir);
        run_checked(cmd, None, "init --bare")?;
        let format = detect_format(&git_dir)?;
        Ok(GitStore { git_dir, format })
    }

    /// The bare git directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The repository's object format.
    pub fn format(&self) -> &ObjectFormat {
        &self.format
    }

    /// Build a `git` command targeting this store with a non-interactive,
    /// hook-free, lock-light environment. `no_lazy` sets `GIT_NO_LAZY_FETCH`.
    fn git(&self, no_lazy: bool) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(&self.git_dir);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        if no_lazy {
            cmd.env("GIT_NO_LAZY_FETCH", "1");
        }
        cmd
    }

    /// Set a config key in the store.
    pub fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let mut cmd = self.git(true);
        cmd.args(["config", key, value]);
        run_checked(cmd, None, "config")?;
        Ok(())
    }

    /// Add a remote.
    pub fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        let mut cmd = self.git(true);
        cmd.args(["remote", "add", name, url]);
        run_checked(cmd, None, "remote add")?;
        Ok(())
    }

    /// Fetch refspecs from a remote with the given options. Configures promisor
    /// settings when a partial-clone filter is used.
    pub fn fetch(&self, remote: &str, refspecs: &[&str], opts: &FetchOptions) -> Result<()> {
        if let Some(filter) = &opts.filter {
            self.set_config(&format!("remote.{remote}.promisor"), "true")?;
            self.set_config(&format!("remote.{remote}.partialclonefilter"), filter)?;
        }
        let mut cmd = self.git(false); // fetch is the scheduler: network allowed
        cmd.arg("fetch");
        if let Some(filter) = &opts.filter {
            cmd.arg(format!("--filter={filter}"));
        }
        if let Some(depth) = opts.depth {
            cmd.arg(format!("--depth={depth}"));
        }
        if !opts.tags {
            cmd.arg("--no-tags");
        }
        cmd.arg(remote);
        for rs in refspecs {
            cmd.arg(rs);
        }
        run_checked(cmd, None, "fetch")?;
        Ok(())
    }

    /// Lazily fetch specific objects into the local store (spec §16). This is
    /// the only entry point allowed to fault objects in over the network.
    pub fn fetch_objects(&self, oids: &[ObjectId]) -> Result<()> {
        if oids.is_empty() {
            return Ok(());
        }
        // Accessing an object with lazy fetch enabled pulls it from the promisor.
        let mut cmd = self.git(false);
        cmd.args(["cat-file", "--batch-check"]);
        let mut input = String::new();
        for oid in oids {
            input.push_str(&oid.to_hex());
            input.push('\n');
        }
        run_checked(cmd, Some(input.as_bytes()), "fetch-objects")?;
        Ok(())
    }

    /// Resolve a ref to an object id. `Ok(None)` if the ref does not exist.
    pub fn resolve_ref(&self, refname: &str) -> Result<Option<ObjectId>> {
        let mut cmd = self.git(true);
        cmd.args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{refname}^{{}}"),
        ]);
        let r = run(cmd, None)?;
        if !r.status_ok {
            return Ok(None);
        }
        let hexs = String::from_utf8_lossy(&r.stdout);
        let hexs = hexs.trim();
        if hexs.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            ObjectId::parse_hex(self.format.clone(), hexs).map_err(|e| {
                Error::new(ErrorCode::Internal, format!("bad oid from rev-parse: {e}"))
            })?,
        ))
    }

    /// Resolve an arbitrary rev expression (e.g. `<commit>^{tree}`) to an oid,
    /// without forcing commit peeling. `Ok(None)` if it does not resolve.
    pub fn rev_parse(&self, expr: &str) -> Result<Option<ObjectId>> {
        let mut cmd = self.git(true);
        cmd.args(["rev-parse", "--verify", "--quiet", expr]);
        let r = run(cmd, None)?;
        if !r.status_ok {
            return Ok(None);
        }
        let hexs = String::from_utf8_lossy(&r.stdout);
        let hexs = hexs.trim();
        if hexs.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            ObjectId::parse_hex(self.format.clone(), hexs).map_err(|e| {
                Error::new(ErrorCode::Internal, format!("bad oid from rev-parse: {e}"))
            })?,
        ))
    }

    /// List refs matching a glob pattern as `(refname, oid)` pairs.
    pub fn for_each_ref(&self, pattern: &str) -> Result<Vec<(String, ObjectId)>> {
        let mut cmd = self.git(true);
        cmd.args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            pattern,
        ]);
        let out = run_checked(cmd, None, "for-each-ref")?;
        let text = String::from_utf8_lossy(&out);
        let mut result = Vec::new();
        for line in text.lines() {
            if let Some((name, oid)) = line.split_once('\u{0}') {
                if let Ok(oid) = ObjectId::parse_hex(self.format.clone(), oid.trim()) {
                    result.push((name.to_string(), oid));
                }
            }
        }
        Ok(result)
    }

    /// Whether an object is present locally (never fetches when `policy` is
    /// cache-only).
    pub fn object_exists(&self, oid: &ObjectId, allow_fetch: bool) -> Result<bool> {
        let mut cmd = self.git(!allow_fetch);
        cmd.args(["cat-file", "-e", &oid.to_hex()]);
        Ok(run(cmd, None)?.status_ok)
    }

    /// Read a tree object and parse it. Honors lazy-fetch policy.
    pub fn read_tree(&self, oid: &ObjectId, allow_fetch: bool) -> Result<TreeObject> {
        let mut cmd = self.git(!allow_fetch);
        cmd.args(["cat-file", "tree", &oid.to_hex()]);
        let r = run(cmd, None)?;
        if !r.status_ok {
            return Err(missing_or_offline(oid, allow_fetch, &r.stderr));
        }
        tree_parse::parse_tree(oid.clone(), &r.stdout, &self.format)
    }

    /// Read raw blob bytes (no working-tree filters). Honors lazy-fetch policy.
    pub fn read_blob_raw(&self, oid: &ObjectId, allow_fetch: bool) -> Result<Vec<u8>> {
        let mut cmd = self.git(!allow_fetch);
        cmd.args(["cat-file", "blob", &oid.to_hex()]);
        let r = run(cmd, None)?;
        if !r.status_ok {
            return Err(missing_or_offline(oid, allow_fetch, &r.stderr));
        }
        Ok(r.stdout)
    }

    /// Apply the configured working-tree (smudge) filters for `path` to a blob,
    /// returning the bytes a normal checkout would write (spec §25). Uses Git's
    /// own filter plumbing.
    ///
    /// `attr_source` is a tree-ish (e.g. the workspace base commit) from which
    /// `.gitattributes` are resolved. This is essential in a *bare* shared store
    /// whose `HEAD` need not match the workspace's base commit (verified
    /// behavior — see docs/feasibility/git-object-fetching.md).
    pub fn smudge_blob(
        &self,
        oid: &ObjectId,
        path: &[u8],
        attr_source: Option<&str>,
        allow_fetch: bool,
    ) -> Result<Vec<u8>> {
        let path_str = std::str::from_utf8(path).map_err(|_| {
            Error::new(
                ErrorCode::InvalidRepositoryPath,
                "non-UTF-8 path cannot be passed to cat-file --path (use raw read)",
            )
        })?;
        let mut cmd = self.git(!allow_fetch);
        if let Some(src) = attr_source {
            cmd.arg(format!("--attr-source={src}"));
        }
        cmd.args([
            "cat-file",
            "--filters",
            &format!("--path={path_str}"),
            &oid.to_hex(),
        ]);
        let r = run(cmd, None)?;
        if !r.status_ok {
            return Err(missing_or_offline(oid, allow_fetch, &r.stderr));
        }
        Ok(r.stdout)
    }

    /// Hash bytes as a blob *with* clean filters for `path` (spec §23:
    /// `git hash-object --path=<path> --stdin`). Pass `write = false` to compute
    /// the oid without writing the object — used by `status`, which must not
    /// persist dirty blobs (spec §2.7). `attr_source` resolves `.gitattributes`
    /// as in [`GitStore::smudge_blob`].
    pub fn hash_blob_clean(
        &self,
        path: &[u8],
        bytes: &[u8],
        attr_source: Option<&str>,
        write: bool,
    ) -> Result<ObjectId> {
        let path_str = std::str::from_utf8(path).map_err(|_| {
            Error::new(
                ErrorCode::InvalidRepositoryPath,
                "non-UTF-8 path cannot be passed to hash-object --path",
            )
        })?;
        let mut cmd = self.git(true);
        if let Some(src) = attr_source {
            cmd.arg(format!("--attr-source={src}"));
        }
        cmd.arg("hash-object");
        if write {
            cmd.arg("-w");
        }
        cmd.args([&format!("--path={path_str}"), "--stdin"]);
        let out = run_checked(cmd, Some(bytes), "hash-object")?;
        self.parse_oid_line(&out)
    }

    /// Hash bytes as a blob with **no** filters (raw). Pass `write = false` to
    /// compute the oid without writing the object.
    pub fn hash_blob_raw(&self, bytes: &[u8], write: bool) -> Result<ObjectId> {
        let mut cmd = self.git(true);
        cmd.arg("hash-object");
        if write {
            cmd.arg("-w");
        }
        cmd.args(["-t", "blob", "--no-filters", "--stdin"]);
        let out = run_checked(cmd, Some(bytes), "hash-object")?;
        self.parse_oid_line(&out)
    }

    /// Write a tree object from entries (canonical byte stream + `hash-object`).
    pub fn write_tree(&self, entries: Vec<glm_core::TreeEntry>) -> Result<ObjectId> {
        let bytes = tree_parse::build_tree_bytes(entries);
        let mut cmd = self.git(true);
        cmd.args(["hash-object", "-w", "-t", "tree", "--stdin"]);
        let out = run_checked(cmd, Some(&bytes), "hash-object tree")?;
        self.parse_oid_line(&out)
    }

    /// Create an ordinary Git commit object (spec §24).
    pub fn commit_tree(&self, params: &CommitParams) -> Result<ObjectId> {
        let mut cmd = self.git(true);
        cmd.arg("commit-tree").arg(params.tree.to_hex());
        for p in &params.parents {
            cmd.arg("-p").arg(p.to_hex());
        }
        if params.sign {
            cmd.arg("-S");
        }
        if let Some(a) = &params.author {
            cmd.env("GIT_AUTHOR_NAME", &a.name);
            cmd.env("GIT_AUTHOR_EMAIL", &a.email);
            if let Some(d) = &a.date {
                cmd.env("GIT_AUTHOR_DATE", d);
            }
        }
        if let Some(c) = &params.committer {
            cmd.env("GIT_COMMITTER_NAME", &c.name);
            cmd.env("GIT_COMMITTER_EMAIL", &c.email);
            if let Some(d) = &c.date {
                cmd.env("GIT_COMMITTER_DATE", d);
            }
        }
        let out = run_checked(cmd, Some(params.message.as_bytes()), "commit-tree")?;
        self.parse_oid_line(&out)
    }

    /// Compare-and-swap a ref (spec §12, §14). `expected_old = None` means
    /// "create" (old value is the null oid).
    pub fn update_ref_cas(
        &self,
        refname: &str,
        new: &ObjectId,
        expected_old: Option<&ObjectId>,
    ) -> Result<()> {
        let mut cmd = self.git(true);
        cmd.args(["update-ref", refname, &new.to_hex()]);
        match expected_old {
            Some(old) => cmd.arg(old.to_hex()),
            None => cmd.arg(ObjectId::null(self.format.clone()).to_hex()),
        };
        let r = run(cmd, None)?;
        if r.status_ok {
            Ok(())
        } else {
            Err(crate::proc::classify(&r.stderr, "update-ref"))
        }
    }

    /// Push a refspec to a remote, optionally with a `--force-with-lease`
    /// compare-and-swap (spec §13 saga step). Returns a classified error on
    /// rejection.
    pub fn push(
        &self,
        remote: &str,
        refspec: &str,
        lease: Option<(&str, Option<&ObjectId>)>,
    ) -> Result<()> {
        let mut cmd = self.git(false);
        cmd.arg("push");
        if let Some((refname, expected)) = lease {
            match expected {
                Some(oid) => cmd.arg(format!("--force-with-lease={refname}:{}", oid.to_hex())),
                None => cmd.arg(format!("--force-with-lease={refname}:")),
            };
        }
        cmd.arg(remote).arg(refspec);
        let r = run(cmd, None)?;
        if r.status_ok {
            Ok(())
        } else {
            Err(crate::proc::classify(&r.stderr, "push"))
        }
    }

    fn parse_oid_line(&self, out: &[u8]) -> Result<ObjectId> {
        let hexs = String::from_utf8_lossy(out);
        ObjectId::parse_hex(self.format.clone(), hexs.trim())
            .map_err(|e| Error::new(ErrorCode::Internal, format!("bad oid output: {e}")))
    }

    /// Spawn a long-lived cat-file batch session for hot reads.
    pub fn batch_session(&self) -> Result<BatchSession> {
        BatchSession::spawn(&self.git_dir, self.format.clone())
    }
}

fn detect_format(git_dir: &Path) -> Result<ObjectFormat> {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir").arg(git_dir);
    cmd.args(["rev-parse", "--show-object-format"]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let out = run_checked(cmd, None, "rev-parse --show-object-format")?;
    let name = String::from_utf8_lossy(&out);
    Ok(ObjectFormat::parse(name.trim()))
}

fn missing_or_offline(oid: &ObjectId, allow_fetch: bool, stderr: &[u8]) -> Error {
    let text = String::from_utf8_lossy(stderr).to_lowercase();
    if allow_fetch {
        if text.contains("could not resolve")
            || text.contains("unable to access")
            || text.contains("connection")
        {
            return Error::new(
                ErrorCode::OfflineMissingObject,
                format!("object {} unavailable: offline", oid.to_hex()),
            )
            .with_action("reconnect and retry, or prefetch while online");
        }
        return Error::new(
            ErrorCode::RemoteMissingObject,
            format!("object {} not found locally or on the remote", oid.to_hex()),
        );
    }
    Error::new(
        ErrorCode::OfflineMissingObject,
        format!(
            "object {} not present locally and fetch not permitted",
            oid.to_hex()
        ),
    )
    .with_action("run with --tree-fetch/hydrate, or reconnect to fetch")
}
