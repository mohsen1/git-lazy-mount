//! `sgrep` — remote code search for lazily-mounted working trees.
//!
//! A content search over a `git-lazy-mount` tree normally reads every file,
//! which faults (materializes) every blob and defeats lazy mounting. `sgrep`
//! answers the query from a **cloud search index** instead — reading zero local
//! files for committed content — while overlaying your uncommitted edits.
//!
//! The cloud backend is abstracted behind [`provider::SearchProvider`]; the
//! built-in providers live in [`providers`] and new ones are easy to add (see
//! [`provider::build`]).

pub mod output;
pub mod overlay;
pub mod provider;
pub mod providers;

pub use provider::{build, Match, Query, SearchError, SearchProvider, NAMES};

use regex::{Regex, RegexBuilder};

/// Compile the user's pattern for the local-overlay grep, mirroring how the
/// provider interprets it (literal vs regex, case sensitivity).
pub fn local_regex(pattern: &str, ignore_case: bool, literal: bool) -> Result<Regex, regex::Error> {
    let p = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    RegexBuilder::new(&p).case_insensitive(ignore_case).build()
}
