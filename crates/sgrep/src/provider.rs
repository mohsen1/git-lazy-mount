//! The cloud code-search provider abstraction.
//!
//! A provider answers a [`Query`] from a remote index, returning [`Match`]es
//! without reading local files. The rest of the tool (CLI, overlay, output) is
//! provider-agnostic, so **adding a backend is a self-contained change**:
//!
//! 1. implement [`SearchProvider`] in `providers/<name>.rs`,
//! 2. add one arm to [`build`] and one entry to [`NAMES`].
//!
//! That's the whole contract — see [`providers::sourcegraph`] for a native
//! example and [`providers::exec`] for a zero-recompile, script-based one.

/// A backend-agnostic code-search request.
#[derive(Debug, Clone)]
pub struct Query {
    /// `OWNER/REPO` — the provider maps this to its own repo identity.
    pub repo: String,
    /// Revision/branch; `None` means the provider's indexed default.
    pub rev: Option<String>,
    /// The search pattern (a regex unless `literal`).
    pub pattern: String,
    /// Optional provider-specific file filter, e.g. a path regex `\.ts$`.
    pub file_filter: Option<String>,
    /// Match case-insensitively.
    pub case_insensitive: bool,
    /// Treat `pattern` literally rather than as a regex.
    pub literal: bool,
    /// Cap on the number of results requested.
    pub max_results: usize,
    /// Optional whole-request timeout in seconds. `None` means provider default.
    pub timeout_secs: Option<u64>,
}

/// A single match: a repo-relative path, a 1-based line number, and the line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Match {
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// Errors a provider (or the registry) can return.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// `--provider`/`SGREP_PROVIDER` named a backend that isn't registered.
    #[error("unknown search provider {0:?}; available: {avail}", avail = NAMES.join(", "))]
    UnknownProvider(String),
    /// The provider could not be configured from the environment.
    #[error("provider configuration: {0}")]
    Config(String),
    /// A network/transport failure talking to the backend.
    #[error("network/transport: {0}")]
    Transport(String),
    /// The backend returned something we couldn't parse.
    #[error("unexpected response: {0}")]
    Protocol(String),
}

/// A remote code-search backend.
pub trait SearchProvider {
    /// Stable identifier, used by `--provider` / `SGREP_PROVIDER`.
    fn name(&self) -> &'static str;
    /// Run the search against the remote index.
    fn search(&self, query: &Query) -> Result<Vec<Match>, SearchError>;
}

/// Provider names known to [`build`]. Add new providers here.
pub const NAMES: &[&str] = &["sourcegraph", "exec"];

/// Construct a provider by name, configured from the environment.
///
/// To register a new provider, add an arm here (and an entry to [`NAMES`]).
pub fn build(name: &str) -> Result<Box<dyn SearchProvider>, SearchError> {
    match name {
        "sourcegraph" => Ok(Box::new(
            crate::providers::sourcegraph::Sourcegraph::from_env()?,
        )),
        "exec" => Ok(Box::new(crate::providers::exec::Exec::from_env()?)),
        // └─ register additional providers here.
        other => Err(SearchError::UnknownProvider(other.to_string())),
    }
}
