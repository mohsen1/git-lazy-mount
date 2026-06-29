//! `sgrep` CLI — remote code search that doesn't materialize the working tree.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use sha2::{Digest, Sha256};

use sgrep::overlay;
use sgrep::provider::{build, Match, Query};
use sgrep::{local_regex, output};

const DEFAULT_COUNT: usize = 100;
const DEFAULT_CACHE_TTL_SECS: u64 = 10 * 60;
const DEFAULT_SOURCEGRAPH_ENDPOINT: &str = "https://sourcegraph.com";

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
    #[arg(long, default_value_t = DEFAULT_COUNT)]
    count: usize,

    /// Search provider (`sourcegraph`, `exec`).
    #[arg(long, env = "SGREP_PROVIDER", default_value = "sourcegraph")]
    provider: String,

    /// Disable the on-disk remote-result cache.
    #[arg(long)]
    no_cache: bool,

    /// Cache TTL for remote results in seconds (`0` disables cache).
    #[arg(long, env = "SGREP_CACHE_TTL_SECS", default_value_t = DEFAULT_CACHE_TTL_SECS)]
    cache_ttl_secs: u64,

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
    let literal = cli.literal || pattern_is_plain_literal(&cli.pattern);
    let query = Query {
        repo,
        rev: cli.rev.clone(),
        pattern: cli.pattern.clone(),
        file_filter: cli.file_filter.clone(),
        case_insensitive: cli.ignore_case,
        literal,
        max_results: cli.count,
    };
    let (remote, cache_status) = search_remote(cli, provider.as_ref(), &query)?;

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
            "[sgrep] {} hits via {} | remote cache: {} | remote files (no fetch): {} | local overlay: {} files ({})",
            results.len(),
            cli.provider,
            cache_status,
            remote_files,
            changed.len(),
            source,
        );
    }
    Ok(!results.is_empty())
}

fn search_remote(
    cli: &Cli,
    provider: &dyn sgrep::SearchProvider,
    query: &Query,
) -> Result<(Vec<Match>, &'static str), Box<dyn std::error::Error>> {
    let cacheable = cli.provider == "sourcegraph" && !cli.no_cache && cli.cache_ttl_secs > 0;
    if !cacheable {
        return Ok((provider.search(query)?, "off"));
    }
    let Some(dir) = cache_dir() else {
        return Ok((provider.search(query)?, "off"));
    };
    let key = cache_key(&cli.provider, query);
    let path = dir.join(format!("{key}.json"));
    if let Some(matches) = read_cache(&path, Duration::from_secs(cli.cache_ttl_secs)) {
        return Ok((matches, "hit"));
    }
    let matches = provider.search(query)?;
    let _ = write_cache(&dir, &path, &matches);
    Ok((matches, "miss"))
}

fn cache_dir() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("SGREP_CACHE_DIR") {
        if !x.is_empty() {
            return Some(PathBuf::from(x));
        }
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("git-lazy-mount").join("sgrep"))
}

fn cache_key(provider: &str, query: &Query) -> String {
    #[derive(serde::Serialize)]
    struct Key<'a> {
        version: u8,
        provider: &'a str,
        provider_endpoint: String,
        provider_token_hash: String,
        repo: &'a str,
        rev: &'a Option<String>,
        pattern: &'a str,
        file_filter: &'a Option<String>,
        case_insensitive: bool,
        literal: bool,
        max_results: usize,
    }
    let token_hash = std::env::var("SRC_ACCESS_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| hex_sha256(s.as_bytes()))
        .unwrap_or_default();
    let key = Key {
        version: 1,
        provider,
        provider_endpoint: std::env::var("SRC_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SOURCEGRAPH_ENDPOINT.to_string()),
        provider_token_hash: token_hash,
        repo: &query.repo,
        rev: &query.rev,
        pattern: &query.pattern,
        file_filter: &query.file_filter,
        case_insensitive: query.case_insensitive,
        literal: query.literal,
        max_results: query.max_results,
    };
    let bytes = serde_json::to_vec(&key).expect("cache key serializes");
    hex_sha256(&bytes)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheFile {
    created_unix_secs: u64,
    matches: Vec<Match>,
}

fn read_cache(path: &Path, ttl: Duration) -> Option<Vec<Match>> {
    let bytes = std::fs::read(path).ok()?;
    let file: CacheFile = serde_json::from_slice(&bytes).ok()?;
    let now = unix_secs(SystemTime::now())?;
    if now.saturating_sub(file.created_unix_secs) > ttl.as_secs() {
        return None;
    }
    Some(file.matches)
}

fn write_cache(dir: &Path, path: &Path, matches: &[Match]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let payload = CacheFile {
        created_unix_secs: unix_secs(SystemTime::now()).unwrap_or(0),
        matches: matches.to_vec(),
    };
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        serde_json::to_writer(&mut f, &payload)?;
        f.write_all(b"\n")?;
    }
    std::fs::rename(tmp, path)
}

fn unix_secs(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn pattern_is_plain_literal(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.bytes().any(|b| {
            matches!(
                b,
                b'\\'
                    | b'^'
                    | b'$'
                    | b'.'
                    | b'|'
                    | b'?'
                    | b'*'
                    | b'+'
                    | b'('
                    | b')'
                    | b'['
                    | b']'
                    | b'{'
                    | b'}'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(max_results: usize) -> Query {
        Query {
            repo: "owner/repo".into(),
            rev: None,
            pattern: "needle".into(),
            file_filter: Some(r"\.rs$".into()),
            case_insensitive: false,
            literal: false,
            max_results,
        }
    }

    fn m(path: &str) -> Match {
        Match {
            path: path.into(),
            line: 7,
            text: "needle here".into(),
        }
    }

    #[test]
    fn cache_key_separates_result_caps() {
        assert_ne!(
            cache_key("sourcegraph", &q(10)),
            cache_key("sourcegraph", &q(100))
        );
    }

    #[test]
    fn cache_round_trips_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entry.json");
        let matches = vec![m("src/lib.rs")];
        write_cache(dir.path(), &path, &matches).unwrap();
        assert_eq!(
            read_cache(&path, Duration::from_secs(DEFAULT_CACHE_TTL_SECS)),
            Some(matches)
        );
    }

    #[test]
    fn stale_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entry.json");
        let payload = CacheFile {
            created_unix_secs: 1,
            matches: vec![m("src/lib.rs")],
        };
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
        assert_eq!(read_cache(&path, Duration::from_secs(1)), None);
    }

    #[test]
    fn plain_identifier_patterns_can_use_literal_search() {
        assert!(pattern_is_plain_literal("setCommand"));
        assert!(pattern_is_plain_literal("NOTIFICATION_READY"));
        assert!(!pattern_is_plain_literal(r"createTypeChecker\b"));
        assert!(!pattern_is_plain_literal("foo|bar"));
        assert!(!pattern_is_plain_literal(r"\.rs$"));
    }
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
