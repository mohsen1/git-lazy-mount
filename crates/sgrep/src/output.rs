//! Result formatting (ripgrep-compatible).

use std::collections::BTreeSet;
use std::io::Write;

use crate::provider::Match;

/// Write matches as `path:line:text`, or as one path per line when `files_only`.
pub fn print_matches(
    matches: &[Match],
    files_only: bool,
    mut w: impl Write,
) -> std::io::Result<()> {
    if files_only {
        let files: BTreeSet<&str> = matches.iter().map(|m| m.path.as_str()).collect();
        for p in files {
            writeln!(w, "{p}")?;
        }
    } else {
        for m in matches {
            writeln!(w, "{}:{}:{}", m.path, m.line, m.text)?;
        }
    }
    Ok(())
}
