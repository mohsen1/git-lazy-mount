//! `sgrep` CLI — remote code search that doesn't materialize the working tree.

use std::path::{Path, PathBuf};

use clap::Parser;

use sgrep::overlay;
use sgrep::provider::{build, Query};
use sgrep::{local_regex, output};

/// Remote code-search grep for lazily-mounted repos (no local materialization).
#[derive(Parser)]
#[command(name = "sgrep", version, about, long_about = None)]
struct Cli {
    /// Search pattern (a regex unless `--literal`).
    pattern: String,

    /// `OWNER/REPO` (default: inferred from the `origin` remote).
    #[arg(long)]
    repo: Option<String>,

    /// Revision/branch (default: the provider's indexed default).
    #[arg(long)]
    rev: Option<String>,

    /// File filter, as a provider path regex, e.g. `\.ts$`.
    #[arg(long = "file")]
    file_filter: Option<String>,

    /// List matching files only (like `rg -l`).
    #[arg(short = 'l', long = "files-with-matches")]
    files_only: bool,

    /// Case-insensitive match.
    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    /// Treat the pattern literally rather than as a regex.
    #[arg(long)]
    literal: bool,

    /// Maximum number of remote results.
    #[arg(long, default_value_t = 1000)]
    count: usize,

    /// Search provider (`sourcegraph`, `exec`).
    #[arg(long, env = "SGREP_PROVIDER", default_value = "sourcegraph")]
    provider: String,

    /// Skip the local-edits overlay (search committed content only).
    #[arg(long)]
    no_overlay: bool,

    /// Explicit locally-changed path to overlay, repo-relative (repeatable).
    /// Skips `git status` — zero blob faults on a cold lazy mount.
    #[arg(long = "changed")]
    changed: Vec<String>,

    /// Read locally-changed paths from FILE (one per line); skips `git status`.
    #[arg(long = "changed-from")]
    changed_from: Option<PathBuf>,

    /// Print a one-line cost summary to stderr.
    #[arg(long)]
    stats: bool,
}

fn main() {
    let cli = Cli::parse();
    match run(&cli) {
        // grep convention: 0 = matched, 1 = no match, 2 = error.
        Ok(found) => std::process::exit(i32::from(!found)),
        Err(e) => {
            if is_broken_pipe(e.as_ref()) {
                std::process::exit(0); // downstream closed (e.g. `| head`)
            }
            eprintln!("sgrep: {e}");
            std::process::exit(2);
        }
    }
}

fn is_broken_pipe(e: &(dyn std::error::Error + 'static)) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
}

/// Returns whether any matches were found.
fn run(cli: &Cli) -> Result<bool, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let root = overlay::repo_root(&cwd);
    let local_repo = root.as_deref().and_then(overlay::infer_repo);
    let repo = cli
        .repo
        .clone()
        .or_else(|| local_repo.clone())
        .ok_or("could not determine repo; pass --repo OWNER/REPO")?;

    let provider = build(&cli.provider)?;
    let query = Query {
        repo,
        rev: cli.rev.clone(),
        pattern: cli.pattern.clone(),
        file_filter: cli.file_filter.clone(),
        case_insensitive: cli.ignore_case,
        literal: cli.literal,
        max_results: cli.count,
    };
    let remote = provider.search(&query)?;

    let base = root.clone().unwrap_or(cwd);
    let (changed, source) =
        resolve_changed(cli, root.as_deref(), &query.repo, local_repo.as_deref())?;
    let remote_files = remote
        .iter()
        .map(|m| m.path.as_str())
        .filter(|p| !changed.iter().any(|c| c == p))
        .collect::<std::collections::HashSet<_>>()
        .len();

    let re = local_regex(&cli.pattern, cli.ignore_case, cli.literal)?;
    let results = overlay::apply(remote, &base, &changed, &re);

    let stdout = std::io::stdout();
    output::print_matches(&results, cli.files_only, stdout.lock())?;

    if cli.stats {
        eprintln!(
            "[sgrep] {} hits via {} | remote files (no fetch): {} | local overlay: {} files ({})",
            results.len(),
            cli.provider,
            remote_files,
            changed.len(),
            source,
        );
    }
    Ok(!results.is_empty())
}

/// Resolve which files to treat as locally-changed for the overlay, and how they
/// were found (for `--stats`).
///
/// Order of preference: explicit `--changed`/`--changed-from`; then, when the
/// local tree *is* the searched repo, the git-lazy-mount change journal (cheap,
/// zero faults); then `git status` (correct anywhere, but faults a cold mount).
fn resolve_changed(
    cli: &Cli,
    root: Option<&Path>,
    repo: &str,
    local_repo: Option<&str>,
) -> Result<(Vec<String>, &'static str), Box<dyn std::error::Error>> {
    if cli.no_overlay {
        return Ok((Vec::new(), "off"));
    }
    if !cli.changed.is_empty() || cli.changed_from.is_some() {
        let mut set = cli.changed.clone();
        if let Some(f) = &cli.changed_from {
            for line in std::fs::read_to_string(f)?.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    set.push(line.to_string());
                }
            }
        }
        set.sort();
        set.dedup();
        return Ok((set, "explicit"));
    }
    match (root, local_repo) {
        (Some(r), Some(lr)) if lr.eq_ignore_ascii_case(repo) => match overlay::glm_changed(r) {
            Some(p) => Ok((p, "journal")),
            None => Ok((overlay::locally_changed(r), "git-status")),
        },
        _ => Ok((Vec::new(), "off")),
    }
}
