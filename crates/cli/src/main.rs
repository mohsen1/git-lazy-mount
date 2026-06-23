//! `git-lazy-mount` — command-line interface (spec §6).
//!
//! Exposes the native lazy-mount commands (Level A; spec §37) against the
//! transactional workspace engine. Inspection commands support `--json` /
//! `--json-lines` with stable envelopes (spec §6).
//!
//! Note: this build provides the headless control + workspace surface. The
//! kernel filesystem projection is the platform FUSE/FSKit/ProjFS backend
//! (feature-gated; see `docs/platform-*.md`). `ls`/`cat` here read through the
//! same workspace engine the kernel backend uses, so behavior is identical.

#![forbid(unsafe_code)]

mod output;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use glm_core::{Error, ErrorCode, FetchPolicy, RepoPath, Result};
use glm_daemon::{CloneOptions, Controller, MountSpec, OpenMount};
use glm_platform::DataRoots;
use output::{Envelope, Format};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "git-lazy-mount",
    version,
    about = "Transactional, Git-backed virtual working copy"
)]
struct Cli {
    /// Emit a single JSON object.
    #[arg(long, global = true)]
    json: bool,
    /// Emit newline-delimited JSON.
    #[arg(long, global = true)]
    json_lines: bool,
    /// Target a specific mountpoint (defaults to the cwd's mount).
    #[arg(long, global = true, value_name = "PATH")]
    mount: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Clone a repository as a lazily-populated workspace (no full checkout).
    Clone(CloneArgs),
    /// List registered mounts.
    List,
    /// Show details about a mount.
    Info,
    /// Unregister a mount.
    Unmount {
        /// The mountpoint to unregister.
        mountpoint: PathBuf,
    },
    /// Show working-tree and staged status (three-tree; O(changed paths)).
    Status,
    /// Show changed paths (name-status).
    Diff,
    /// Stage paths.
    Add {
        /// Paths to stage (repo-root-relative).
        pathspec: Vec<String>,
        /// Stage all working-tree changes.
        #[arg(short = 'A', long)]
        all: bool,
    },
    /// Unstage paths.
    Unstage {
        /// Paths to unstage.
        pathspec: Vec<String>,
    },
    /// Restore working-tree paths from the base (drop overlay edits).
    Restore {
        /// Paths to restore.
        pathspec: Vec<String>,
    },
    /// Create a commit from the staged delta.
    Commit {
        /// Commit message.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Read the message from a file.
        #[arg(long, value_name = "PATH")]
        message_file: Option<PathBuf>,
    },
    /// Push the attached branch to the remote (compare-and-swap).
    Push,
    /// List local branches.
    Branch,
    /// Switch the base revision (clean workspace only).
    Switch {
        /// Branch name or commit to switch to.
        rev: String,
    },
    /// Reset HEAD (soft/mixed; hard is not implemented).
    Reset {
        /// Move HEAD only; keep stage and working tree.
        #[arg(long)]
        soft: bool,
        /// Move HEAD and reset the stage (default).
        #[arg(long)]
        mixed: bool,
        /// Replace working state (not implemented).
        #[arg(long)]
        hard: bool,
        /// Target revision (defaults to current HEAD).
        rev: Option<String>,
    },
    /// Merge a revision into the current base (clean workspace only).
    Merge {
        /// Branch name or commit to merge.
        rev: String,
    },
    /// Run stock `git` against the lazy store: read-only inspection (status,
    /// log, show, diff, …) and native `git commit`. Staging is done with
    /// `git lazy-mount add`. Use `--` to separate git arguments from
    /// lazy-mount flags, e.g. `git lazy-mount git -- log --oneline`.
    Git {
        /// Arguments passed to git (the subcommand and its options).
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<OsString>,
    },
    /// List a directory through the workspace projection.
    Ls {
        /// Directory to list (defaults to the root).
        path: Option<String>,
    },
    /// Read a file's working-tree content to stdout.
    Cat {
        /// File to read.
        path: String,
    },
    /// Materialize (fetch) content for paths.
    Hydrate {
        /// Paths to hydrate.
        pathspec: Vec<String>,
    },
    /// Prefetch content for paths.
    Prefetch {
        /// Paths to prefetch.
        pathspec: Vec<String>,
    },
    /// Show object-fetch / hydration statistics.
    Stats,
    /// Cache subcommands.
    #[command(subcommand)]
    Cache(CacheCmd),
    /// Operation-log subcommands.
    #[command(subcommand)]
    Op(OpCmd),
    /// Diagnose environment and mount health.
    Doctor,
    /// Check workspace consistency and report (does not mutate user data).
    Fsck,
    /// Trust subcommands (run repository-provided filters/hooks).
    #[command(subcommand)]
    Trust(TrustCmd),
    /// Debug helpers (drive the workspace engine without a kernel mount).
    #[command(subcommand)]
    Debug(DebugCmd),
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Write stdin to a path in the working overlay (what an editor save does
    /// through the FUSE backend).
    Write {
        /// The repo-root-relative path to write.
        path: String,
        /// Mark the file executable.
        #[arg(long)]
        executable: bool,
    },
    /// Delete a path in the working tree (tombstone).
    Rm {
        /// The path to delete.
        path: String,
    },
    /// Rename a path in the working tree.
    Mv {
        /// Source path.
        from: String,
        /// Destination path.
        to: String,
    },
}

#[derive(Args)]
struct CloneArgs {
    /// Repository URL or local path.
    url: String,
    /// Mountpoint directory.
    mountpoint: PathBuf,
    /// Branch to attach to.
    #[arg(long)]
    branch: Option<String>,
    /// Partial-clone filter (default: blob:none).
    #[arg(long, default_value = "blob:none")]
    filter: String,
    /// Shallow depth.
    #[arg(long)]
    depth: Option<u32>,
    /// Permit a full-object clone if the remote rejects the filter.
    #[arg(long)]
    allow_full_object_clone: bool,
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Show cache statistics.
    Stats,
}

#[derive(Subcommand)]
enum OpCmd {
    /// Show the operation log.
    Log {
        /// Maximum entries.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum TrustCmd {
    /// Show the current mount's trust status.
    Show,
    /// Grant trust to the current mount's repository.
    Grant,
    /// Revoke trust from the current mount's repository.
    Revoke,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GLM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let cli = Cli::parse();
    let format = if cli.json {
        Format::Json
    } else if cli.json_lines {
        Format::JsonLines
    } else {
        Format::Human
    };

    match run(&cli, format) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if format == Format::Human {
                eprintln!("error: {e}");
                if let Some(action) = &e.recommended_action {
                    eprintln!("hint: {action}");
                }
            } else {
                Envelope::new("error", json!(e.to_json())).print_json();
            }
            match e.code {
                ErrorCode::MountLifecycle => ExitCode::from(3),
                ErrorCode::OfflineMissingObject => ExitCode::from(4),
                _ => ExitCode::FAILURE,
            }
        }
    }
}

fn controller() -> Controller {
    match std::env::var_os("GLM_DATA_ROOT") {
        Some(root) => Controller::new(DataRoots::ephemeral(root)),
        None => Controller::for_user(),
    }
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn open_mount(cli: &Cli) -> Result<OpenMount> {
    let ctl = controller();
    let spec = ctl.resolve_mount(cli.mount.as_deref(), &cwd())?;
    ctl.open(&spec, None)
}

fn repo_path(s: &str) -> Result<RepoPath> {
    RepoPath::from_bytes(s.as_bytes().to_vec()).map_err(|e| {
        Error::new(
            ErrorCode::InvalidRepositoryPath,
            format!("invalid path '{s}': {e}"),
        )
    })
}

const POLICY: FetchPolicy = FetchPolicy::AllowNetwork;

fn run(cli: &Cli, format: Format) -> Result<()> {
    match &cli.command {
        Command::Clone(args) => cmd_clone(args, format),
        Command::List => cmd_list(format),
        Command::Info => cmd_info(cli, format),
        Command::Unmount { mountpoint } => cmd_unmount(mountpoint, format),
        Command::Status | Command::Diff => cmd_status(cli, format),
        Command::Add { pathspec, all } => cmd_add(cli, pathspec, *all, format),
        Command::Unstage { pathspec } => cmd_unstage(cli, pathspec, format),
        Command::Restore { pathspec } => cmd_restore(cli, pathspec, format),
        Command::Commit {
            message,
            message_file,
        } => cmd_commit(cli, message.clone(), message_file.clone(), format),
        Command::Push => cmd_push(cli, format),
        Command::Branch => cmd_branch(cli, format),
        Command::Switch { rev } => cmd_switch(cli, rev, format),
        Command::Reset {
            soft,
            mixed,
            hard,
            rev,
        } => cmd_reset(cli, *soft, *mixed, *hard, rev.clone(), format),
        Command::Merge { rev } => cmd_merge(cli, rev, format),
        Command::Git { args } => cmd_git(cli, args, format),
        Command::Ls { path } => cmd_ls(cli, path.clone(), format),
        Command::Cat { path } => cmd_cat(cli, path),
        Command::Hydrate { pathspec } | Command::Prefetch { pathspec } => {
            cmd_hydrate(cli, pathspec, format)
        }
        Command::Stats | Command::Cache(CacheCmd::Stats) => cmd_stats(cli, format),
        Command::Op(OpCmd::Log { limit }) => cmd_op_log(cli, *limit, format),
        Command::Doctor => cmd_doctor(cli, format),
        Command::Fsck => cmd_fsck(cli, format),
        Command::Trust(t) => cmd_trust(cli, t, format),
        Command::Debug(d) => cmd_debug(cli, d, format),
    }
}

fn cmd_debug(cli: &Cli, d: &DebugCmd, format: Format) -> Result<()> {
    use std::io::Read;
    let mount = open_mount(cli)?;
    match d {
        DebugCmd::Write { path, executable } => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            mount
                .workspace
                .write_full(&repo_path(path)?, &buf, *executable)?;
            emit_simple(
                format,
                "debug-write",
                json!({ "path": path, "bytes": buf.len() }),
                &format!("wrote {} bytes to {path}", buf.len()),
            );
        }
        DebugCmd::Rm { path } => {
            mount.workspace.delete(&repo_path(path)?, POLICY)?;
            emit_simple(
                format,
                "debug-rm",
                json!({ "path": path }),
                &format!("deleted {path}"),
            );
        }
        DebugCmd::Mv { from, to } => {
            mount
                .workspace
                .rename(&repo_path(from)?, &repo_path(to)?, POLICY)?;
            emit_simple(
                format,
                "debug-mv",
                json!({ "from": from, "to": to }),
                &format!("renamed {from} -> {to}"),
            );
        }
    }
    Ok(())
}

fn cmd_clone(args: &CloneArgs, format: Format) -> Result<()> {
    let ctl = controller();
    let opts = CloneOptions {
        filter: Some(args.filter.clone()),
        branch: args.branch.clone(),
        depth: args.depth,
        allow_full_object_clone: args.allow_full_object_clone,
        identity: None,
    };
    let spec = ctl.clone_repo(&args.url, &args.mountpoint, &opts)?;
    if format == Format::Human {
        println!("Cloned (lazily) into {}", spec.mountpoint.display());
        println!("  repo:   {}", spec.repo_id);
        println!(
            "  branch: {}",
            spec.attached_branch.as_deref().unwrap_or("-")
        );
        println!("  filter: {}", spec.filter.as_deref().unwrap_or("(none)"));
        println!("No files were checked out; content is fetched on access.");
    } else {
        Envelope::new(
            "clone",
            json!({
                "mountpoint": spec.mountpoint.display().to_string(),
                "repo_id": spec.repo_id,
                "attached_branch": spec.attached_branch,
                "filter": spec.filter,
                "store_dir": spec.store_dir.display().to_string(),
            }),
        )
        .workspace(spec.id)
        .print_json();
    }
    Ok(())
}

fn cmd_list(format: Format) -> Result<()> {
    let mounts = controller().list()?;
    if format == Format::Human {
        if mounts.is_empty() {
            println!("No mounts registered.");
        }
        for m in &mounts {
            println!(
                "{}  [{}]  {}",
                m.mountpoint.display(),
                state_str(m),
                m.repo_id
            );
        }
    } else {
        let arr: Vec<_> = mounts.iter().map(mount_json).collect();
        Envelope::new("list", json!(arr)).print_json();
    }
    Ok(())
}

fn cmd_info(cli: &Cli, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let base = mount.workspace.base_commit().map(|c| c.to_hex());
    if format == Format::Human {
        println!("Mount:   {}", mount.spec.mountpoint.display());
        println!("Repo:    {}", mount.spec.repo_id);
        println!(
            "Branch:  {}",
            mount.spec.attached_branch.as_deref().unwrap_or("-")
        );
        println!("Base:    {}", base.as_deref().unwrap_or("(none)"));
        println!("Store:   {}", mount.spec.store_dir.display());
    } else {
        Envelope::new(
            "info",
            json!({
                "mount": mount_json(&mount.spec),
                "base_commit": base,
                "stale": mount.workspace.oplog().is_stale().unwrap_or(false),
            }),
        )
        .workspace(mount.spec.id)
        .print_json();
    }
    Ok(())
}

fn cmd_unmount(mountpoint: &std::path::Path, format: Format) -> Result<()> {
    let removed = controller().unmount(mountpoint)?;
    emit_simple(
        format,
        "unmount",
        json!({ "removed": removed }),
        if removed {
            "unmounted"
        } else {
            "no such mount"
        },
    );
    Ok(())
}

fn cmd_status(cli: &Cli, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let status = mount.workspace.status(POLICY)?;
    match format {
        Format::Human => {
            if status.is_empty() {
                println!("clean");
            }
            for e in &status {
                println!(
                    "{}{} {}",
                    e.index.letter(),
                    e.worktree.letter(),
                    e.path.escape()
                );
            }
        }
        Format::JsonLines => {
            for e in &status {
                println!("{}", serde_json::to_string(e).unwrap_or_default());
            }
        }
        Format::Json => {
            Envelope::new("status", json!(status))
                .workspace(mount.spec.id)
                .print_json();
        }
    }
    Ok(())
}

fn cmd_add(cli: &Cli, pathspec: &[String], all: bool, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let count = if all {
        mount.workspace.stage_all(POLICY)?
    } else {
        for s in pathspec {
            mount.workspace.stage_path(&repo_path(s)?, POLICY)?;
        }
        pathspec.len()
    };
    emit_simple(
        format,
        "add",
        json!({ "staged": count }),
        &format!("staged {count} path(s)"),
    );
    Ok(())
}

fn cmd_unstage(cli: &Cli, pathspec: &[String], format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    for s in pathspec {
        mount.workspace.unstage(&repo_path(s)?)?;
    }
    emit_simple(
        format,
        "unstage",
        json!({ "unstaged": pathspec.len() }),
        "unstaged",
    );
    Ok(())
}

fn cmd_restore(cli: &Cli, pathspec: &[String], format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    for s in pathspec {
        mount.workspace.restore_worktree(&repo_path(s)?)?;
    }
    emit_simple(
        format,
        "restore",
        json!({ "restored": pathspec.len() }),
        "restored",
    );
    Ok(())
}

fn cmd_commit(
    cli: &Cli,
    message: Option<String>,
    message_file: Option<PathBuf>,
    format: Format,
) -> Result<()> {
    let mount = open_mount(cli)?;
    let message = match (message, message_file) {
        (Some(m), _) => m,
        (None, Some(f)) => std::fs::read_to_string(&f)?,
        (None, None) => {
            return Err(
                Error::new(ErrorCode::Configuration, "a commit message is required")
                    .with_action("pass -m <msg> or --message-file <path>"),
            )
        }
    };
    let outcome = mount.workspace.commit(&message, POLICY)?;
    if format == Format::Human {
        println!(
            "[{}] {}",
            short(&outcome.commit.to_hex()),
            first_line(&message)
        );
        if !outcome.branch_advanced {
            eprintln!(
                "warning: attached branch diverged; commit kept on the workspace ref \
                 (push/merge to reconcile)"
            );
        }
    } else {
        let mut env = Envelope::new(
            "commit",
            json!({
                "commit": outcome.commit.to_hex(),
                "branch_advanced": outcome.branch_advanced,
            }),
        )
        .workspace(mount.spec.id);
        env.operation_id = Some(outcome.operation.to_hex());
        if let Some(div) = &outcome.divergence {
            env = env.warn(format!(
                "attached branch diverged ({}); workspace commit kept on its private ref",
                div.summary
            ));
        }
        env.print_json();
    }
    Ok(())
}

fn cmd_push(cli: &Cli, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    mount.workspace.push(POLICY)?;
    emit_simple(format, "push", json!({ "pushed": true }), "pushed");
    Ok(())
}

fn resolve_rev(mount: &OpenMount, rev: &str) -> Result<glm_core::ObjectId> {
    mount.store.rev_parse(rev)?.ok_or_else(|| {
        Error::new(
            ErrorCode::Configuration,
            format!("cannot resolve revision '{rev}'"),
        )
    })
}

fn cmd_merge(cli: &Cli, rev: &str, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let theirs = resolve_rev(&mount, rev)?;
    match mount.workspace.merge(theirs)? {
        glm_workspace::MergeResult::Clean { commit, operation } => {
            let mut env = Envelope::new(
                "merge",
                json!({ "status": "clean", "commit": commit.to_hex() }),
            )
            .workspace(mount.spec.id);
            env.operation_id = Some(operation.to_hex());
            if format == Format::Human {
                println!("merged cleanly: {}", short(&commit.to_hex()));
            } else {
                env.print_json();
            }
        }
        glm_workspace::MergeResult::Conflicts { paths, messages } => {
            if format == Format::Human {
                println!("merge produced conflicts in {} path(s):", paths.len());
                for p in &paths {
                    println!("  {}", p.escape());
                }
                for m in &messages {
                    println!("  {m}");
                }
                println!("resolve, then `git lazy-mount add` and `commit`.");
            } else {
                let arr: Vec<_> = paths.iter().map(|p| p.escape()).collect();
                Envelope::new(
                    "merge",
                    json!({ "status": "conflicts", "paths": arr, "messages": messages }),
                )
                .workspace(mount.spec.id)
                .warn("merge conflicts; resolve and commit")
                .print_json();
            }
        }
    }
    Ok(())
}

/// Git subcommands that only read repository state and are safe to run through
/// the interop bridge (default-deny: everything else is rejected so that, e.g.,
/// `git gc`/`git prune` can never reach the shared object store).
const BRIDGE_READ_ONLY: &[&str] = &[
    "status",
    "log",
    "show",
    "diff",
    "diff-tree",
    "diff-index",
    "diff-files",
    "ls-files",
    "ls-tree",
    "cat-file",
    "rev-parse",
    "rev-list",
    "blame",
    "annotate",
    "shortlog",
    "describe",
    "for-each-ref",
    "whatchanged",
    "grep",
    "name-rev",
    "merge-base",
    "symbolic-ref",
    "show-ref",
    "show-branch",
    "count-objects",
    "verify-commit",
    "verify-tag",
    "cherry",
    "range-diff",
    "var",
    "check-ignore",
    "check-attr",
];

/// Determine the git subcommand (the verb) from the bridge args, skipping git's
/// global options and the values of those that take one.
fn bridge_verb(args: &[OsString]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].to_string_lossy();
        if let Some(long) = a.strip_prefix("--") {
            // `--opt=value` is self-contained; a few long options take a
            // separate value, the rest are flags.
            if long.contains('=') {
                i += 1;
            } else if matches!(long, "git-dir" | "work-tree" | "namespace" | "super-prefix") {
                i += 2;
            } else {
                i += 1;
            }
        } else if a.starts_with('-') {
            // `-C <path>` and `-c <name=value>` take a value.
            if a == "-C" || a == "-c" {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            return Some(a.into_owned());
        }
    }
    None
}

/// Reject `git commit` flags the bridge cannot honor because the working tree
/// is virtual (`-a`/`--all`, `--patch`, `--interactive`, `--amend`, …).
fn reject_unsupported_commit_flags(args: &[OsString]) -> Result<()> {
    for a in args {
        let s = a.to_string_lossy();
        let explicit = matches!(
            s.as_ref(),
            "-a" | "--all"
                | "-p"
                | "--patch"
                | "--interactive"
                | "--amend"
                | "-i"
                | "--include"
                | "-o"
                | "--only"
        );
        // Combined short flags such as `-am` that contain `a`.
        let short_with_a = s.starts_with('-') && !s.starts_with("--") && s.contains('a');
        if explicit || short_with_a {
            return Err(Error::new(
                ErrorCode::UnsupportedOperation,
                format!(
                    "`git commit {s}` is not supported through the bridge (the working tree is virtual)"
                ),
            )
            .with_action("stage with `git lazy-mount add`, then `git lazy-mount git commit -m ...`"));
        }
    }
    Ok(())
}

/// Classify the verb: `Ok(())` for the read-only allowlist and `commit`;
/// otherwise a helpful error pointing at the native `git lazy-mount` command.
fn classify_bridge_verb(verb: &str) -> Result<()> {
    if verb == "commit" || BRIDGE_READ_ONLY.contains(&verb) {
        return Ok(());
    }
    let hint: Option<&str> = match verb {
        "add" => Some("stage with `git lazy-mount add`"),
        "rm" => Some("delete through the mount, then `git lazy-mount add`"),
        "mv" => Some("rename through the mount, then `git lazy-mount add`"),
        "reset" => Some("use `git lazy-mount reset`"),
        "restore" | "checkout" => Some("use `git lazy-mount restore` / `git lazy-mount switch`"),
        "switch" => Some("use `git lazy-mount switch`"),
        "merge" => Some("use `git lazy-mount merge`"),
        "push" => Some("use `git lazy-mount push`"),
        "branch" | "tag" => Some(
            "refs created in the bridge do not affect the workspace; use `git lazy-mount branch`",
        ),
        "gc" | "prune" | "repack" | "pack-objects" | "pack-refs" | "filter-branch" => {
            Some("object maintenance on the shared store is not allowed through the bridge")
        }
        _ => None,
    };
    let mut e = Error::new(
        ErrorCode::UnsupportedOperation,
        format!(
            "`git {verb}` is not supported through the interop bridge \
             (read-only commands and `commit` are)"
        ),
    );
    if let Some(h) = hint {
        e = e.with_action(h);
    }
    Err(e)
}

fn cmd_git(cli: &Cli, args: &[OsString], format: Format) -> Result<()> {
    if args.is_empty() {
        return Err(
            Error::new(ErrorCode::Configuration, "no git command given").with_action(
                "e.g. `git lazy-mount git status` or `git lazy-mount git -- log --oneline`",
            ),
        );
    }
    let verb = bridge_verb(args).ok_or_else(|| {
        Error::new(
            ErrorCode::Configuration,
            "could not determine the git subcommand",
        )
        .with_action("put the subcommand first, e.g. `git lazy-mount git status`")
    })?;
    classify_bridge_verb(&verb)?;

    let is_commit = verb == "commit";
    if is_commit {
        reject_unsupported_commit_flags(args)?;
    }

    let mount = open_mount(cli)?;
    let base = mount
        .workspace
        .base_commit()
        .ok_or_else(|| Error::new(ErrorCode::Configuration, "workspace has no base commit yet"))?;
    let index_tree = mount.workspace.staged_tree(POLICY)?;
    let scratch = mount.spec.ws_dir.join("interop");

    let outcome = mount.store.interop_run(
        &scratch,
        &base,
        mount.spec.attached_branch.as_deref(),
        Some(&index_tree),
        args,
    )?;

    // Adopt a commit produced through the bridge into the workspace.
    if is_commit && outcome.status.success() {
        if let Some(new_head) = &outcome.head {
            if *new_head != base {
                let res = mount.workspace.adopt_commit(new_head.clone(), POLICY)?;
                if format == Format::Human {
                    eprintln!(
                        "git-lazy-mount: adopted {} as the new workspace base{}",
                        short(&res.commit.to_hex()),
                        if res.branch_advanced {
                            ""
                        } else {
                            " (attached branch diverged; kept on the workspace ref)"
                        }
                    );
                } else {
                    let mut env = Envelope::new(
                        "git-commit",
                        json!({
                            "commit": res.commit.to_hex(),
                            "branch_advanced": res.branch_advanced,
                        }),
                    )
                    .workspace(mount.spec.id.clone());
                    env.operation_id = Some(res.operation.to_hex());
                    env.print_json();
                }
            }
        }
    }

    // Propagate git's own exit code so scripts see the real result.
    if !outcome.status.success() {
        std::process::exit(outcome.status.code().unwrap_or(1));
    }
    Ok(())
}

fn cmd_branch(cli: &Cli, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let branches = mount.workspace.list_branches()?;
    let current = mount.spec.attached_branch.clone();
    if format == Format::Human {
        for (name, oid) in &branches {
            let marker = if Some(name) == current.as_ref() {
                "* "
            } else {
                "  "
            };
            println!("{marker}{}  {}", short(&oid.to_hex()), name);
        }
    } else {
        let arr: Vec<_> = branches
            .iter()
            .map(|(n, o)| json!({ "ref": n, "oid": o.to_hex(), "attached": Some(n) == current.as_ref() }))
            .collect();
        Envelope::new("branch", json!(arr)).print_json();
    }
    Ok(())
}

fn cmd_switch(cli: &Cli, rev: &str, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let target = resolve_rev(&mount, rev)?;
    let op = mount.workspace.switch(target.clone())?;
    let mut env =
        Envelope::new("switch", json!({ "base": target.to_hex() })).workspace(mount.spec.id);
    env.operation_id = Some(op.to_hex());
    if format == Format::Human {
        println!("switched to {}", short(&target.to_hex()));
    } else {
        env.print_json();
    }
    Ok(())
}

fn cmd_reset(
    cli: &Cli,
    soft: bool,
    _mixed: bool,
    hard: bool,
    rev: Option<String>,
    format: Format,
) -> Result<()> {
    let mount = open_mount(cli)?;
    let mode = if hard {
        glm_workspace::ResetMode::Hard
    } else if soft {
        glm_workspace::ResetMode::Soft
    } else {
        glm_workspace::ResetMode::Mixed
    };
    let target = match rev {
        Some(r) => resolve_rev(&mount, &r)?,
        None => mount
            .workspace
            .base_commit()
            .ok_or_else(|| Error::new(ErrorCode::Configuration, "no HEAD to reset"))?,
    };
    let op = mount.workspace.reset(mode, target.clone())?;
    let mut env = Envelope::new(
        "reset",
        json!({ "mode": format!("{mode:?}").to_lowercase(), "base": target.to_hex() }),
    )
    .workspace(mount.spec.id);
    env.operation_id = Some(op.to_hex());
    if format == Format::Human {
        println!("reset ({:?}) to {}", mode, short(&target.to_hex()));
    } else {
        env.print_json();
    }
    Ok(())
}

fn cmd_ls(cli: &Cli, path: Option<String>, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let dir = match path {
        Some(p) => repo_path(&p)?,
        None => RepoPath::root(),
    };
    let entries = mount.workspace.list_dir(&dir, POLICY)?;
    if format == Format::Human {
        for e in &entries {
            let suffix = match e.kind {
                glm_workspace::EntryKind::Dir => "/",
                glm_workspace::EntryKind::Symlink => "@",
                glm_workspace::EntryKind::File { executable: true } => "*",
                _ => "",
            };
            println!("{}{}", String::from_utf8_lossy(&e.name), suffix);
        }
    } else {
        let arr: Vec<_> = entries
            .iter()
            .map(|e| {
                json!({
                    "name": String::from_utf8_lossy(&e.name),
                    "kind": format!("{:?}", e.kind),
                })
            })
            .collect();
        Envelope::new("ls", json!(arr)).print_json();
    }
    Ok(())
}

fn cmd_cat(cli: &Cli, path: &str) -> Result<()> {
    use std::io::Write;
    let mount = open_mount(cli)?;
    let bytes = mount.workspace.read_file(&repo_path(path)?, POLICY)?;
    std::io::stdout().write_all(&bytes)?;
    Ok(())
}

fn cmd_hydrate(cli: &Cli, pathspec: &[String], format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let mut bytes = 0u64;
    for s in pathspec {
        bytes += mount.workspace.read_file(&repo_path(s)?, POLICY)?.len() as u64;
    }
    let m = mount.workspace.provider().metrics();
    emit_simple(
        format,
        "hydrate",
        json!({ "paths": pathspec.len(), "bytes": bytes, "objects_fetched": m.objects_fetched }),
        &format!("hydrated {} path(s), {} bytes", pathspec.len(), bytes),
    );
    Ok(())
}

fn cmd_stats(cli: &Cli, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let m = mount.workspace.provider().metrics();
    let result = json!({
        "tree_reads": m.tree_reads,
        "blob_reads": m.blob_reads,
        "filtered_reads": m.filtered_reads,
        "bytes_read": m.bytes_read,
        "presence_checks": m.presence_checks,
        "fetch_invocations": m.fetch_invocations,
        "objects_fetched": m.objects_fetched,
        "coalesced_waits": m.coalesced_waits,
    });
    if format == Format::Human {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        );
    } else {
        Envelope::new("stats", result)
            .workspace(mount.spec.id)
            .print_json();
    }
    Ok(())
}

fn cmd_op_log(cli: &Cli, limit: usize, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let ops = mount.workspace.oplog().log(limit)?;
    if format == Format::Human {
        for op in &ops {
            println!("{}  {}", short(&op.id.to_hex()), op.description);
        }
    } else {
        let arr: Vec<_> = ops
            .iter()
            .map(|o| {
                json!({
                    "id": o.id.to_hex(),
                    "description": o.description,
                    "timestamp": o.timestamp_unix,
                })
            })
            .collect();
        Envelope::new("op-log", json!(arr)).print_json();
    }
    Ok(())
}

fn cmd_doctor(cli: &Cli, format: Format) -> Result<()> {
    let git_ok = std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let mut checks = vec![json!({ "check": "git_available", "ok": git_ok })];
    if let Ok(mount) = open_mount(cli) {
        let report = mount.workspace.oplog().recover()?;
        checks
            .push(json!({ "check": "oplog_healthy", "ok": report.healthy, "stale": report.stale }));
    }
    if format == Format::Human {
        for c in &checks {
            println!("{c}");
        }
    } else {
        Envelope::new("doctor", json!(checks)).print_json();
    }
    Ok(())
}

fn cmd_fsck(cli: &Cli, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let report = mount.workspace.oplog().recover()?;
    if format == Format::Human {
        println!("healthy: {}  stale: {}", report.healthy, report.stale);
        for issue in &report.issues {
            println!("  issue: {issue}");
        }
    } else {
        Envelope::new(
            "fsck",
            json!({
                "healthy": report.healthy,
                "stale": report.stale,
                "issues": report.issues,
                "current_op": report.current_op.map(|o| o.to_hex()),
            }),
        )
        .workspace(mount.spec.id)
        .print_json();
    }
    Ok(())
}

fn cmd_trust(cli: &Cli, t: &TrustCmd, format: Format) -> Result<()> {
    let mount = open_mount(cli)?;
    let trust_file = match std::env::var_os("GLM_DATA_ROOT") {
        Some(r) => DataRoots::ephemeral(r).config.join("trust.json"),
        None => DataRoots::for_user().config.join("trust.json"),
    };
    let store = glm_filters::TrustStore::open(trust_file)?;
    let repo = glm_core::RepoId(mount.spec.repo_id.clone());
    let (action, trusted) = match t {
        TrustCmd::Show => ("show", store.is_trusted(&repo)),
        TrustCmd::Grant => {
            store.grant(&repo)?;
            ("grant", true)
        }
        TrustCmd::Revoke => {
            store.revoke(&repo)?;
            ("revoke", false)
        }
    };
    emit_simple(
        format,
        "trust",
        json!({ "action": action, "repo_id": mount.spec.repo_id, "trusted": trusted }),
        &format!("{action}: trusted={trusted}"),
    );
    Ok(())
}

fn emit_simple(format: Format, command: &str, result: serde_json::Value, human: &str) {
    if format == Format::Human {
        println!("{human}");
    } else {
        Envelope::new(command, result).print_json();
    }
}

fn mount_json(m: &MountSpec) -> serde_json::Value {
    json!({
        "id": m.id,
        "mountpoint": m.mountpoint.display().to_string(),
        "repo_id": m.repo_id,
        "attached_branch": m.attached_branch,
        "filter": m.filter,
        "state": state_str(m),
    })
}

fn state_str(m: &MountSpec) -> String {
    format!("{:?}", m.state).to_lowercase()
}

fn short(hex: &str) -> &str {
    &hex[..hex.len().min(10)]
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}
