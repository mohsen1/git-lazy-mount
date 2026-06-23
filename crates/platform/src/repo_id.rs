//! Canonical repository identity, derived **without embedding credentials**
//! (spec §8).
//!
//! Two URLs that address the same repository over different transports
//! (`https://github.com/o/r.git` and `git@github.com:o/r`) and with different
//! embedded credentials must yield the *same* [`RepoId`], so a single shared
//! object store can back multiple mounts (spec §2.3). Credentials never appear
//! in the identity or on disk.

use glm_core::RepoId;
use sha2::{Digest, Sha256};

/// Compute the canonical, credential-free identity string for a repository URL
/// or local path.
///
/// The result intentionally ignores the scheme and any userinfo, normalizes the
/// host to lowercase, drops default ports, and strips a trailing `.git` and
/// slashes from the path.
pub fn canonical_identity(url: &str) -> String {
    let url = url.trim();

    // scp-like ssh syntax: `[user@]host:path` (no `://`, colon before slash).
    if !url.contains("://") {
        if let Some(idx) = scp_colon(url) {
            let host = strip_userinfo(&url[..idx]);
            let path = &url[idx + 1..];
            return format!("{}/{}", host.to_ascii_lowercase(), normalize_path(path));
        }
        // A bare local path.
        return format!("file/{}", normalize_path(url));
    }

    // scheme://[userinfo@]host[:port]/path
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let (authority, path) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], &after_scheme[i + 1..]),
        None => (after_scheme, ""),
    };
    let authority = strip_userinfo(authority);
    let host = drop_default_port(authority).to_ascii_lowercase();

    if host.is_empty() {
        // e.g. file:///abs/path — treat the path as a local identity.
        return format!("file/{}", normalize_path(path));
    }
    format!("{}/{}", host, normalize_path(path))
}

/// Derive the on-disk [`RepoId`] from a URL or local path.
///
/// The id is a filesystem-safe slug plus a short hash of the canonical identity,
/// so it is both human-recognizable and collision-resistant.
pub fn repo_id(url: &str) -> RepoId {
    let canonical = canonical_identity(url);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hex::encode(hasher.finalize());
    let slug = slugify(&canonical);
    RepoId(format!("{}-{}", slug, &digest[..12]))
}

fn scp_colon(url: &str) -> Option<usize> {
    let colon = url.find(':')?;
    let slash = url.find('/').unwrap_or(usize::MAX);
    if colon < slash {
        Some(colon)
    } else {
        None
    }
}

fn strip_userinfo(authority: &str) -> &str {
    match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    }
}

fn drop_default_port(authority: &str) -> &str {
    for default in [":443", ":22", ":80", ":9418"] {
        if let Some(stripped) = authority.strip_suffix(default) {
            return stripped;
        }
    }
    authority
}

fn normalize_path(path: &str) -> String {
    let p = path.trim_matches('/');
    let p = p.strip_suffix(".git").unwrap_or(p);
    p.trim_end_matches('/').to_string()
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    // Cap length so directory names stay reasonable.
    trimmed.chars().take(48).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_and_ssh_match() {
        let a = canonical_identity("https://github.com/example/huge-repo.git");
        let b = canonical_identity("git@github.com:example/huge-repo.git");
        let c = canonical_identity("https://github.com/example/huge-repo");
        assert_eq!(a, "github.com/example/huge-repo");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn credentials_are_stripped() {
        let a = canonical_identity("https://user:secrettoken@github.com/o/r.git");
        assert_eq!(a, "github.com/o/r");
        assert!(!a.contains("secrettoken"));
        // And they never leak into the id.
        let id = repo_id("https://user:secrettoken@github.com/o/r.git");
        assert!(!id.0.contains("secrettoken"));
    }

    #[test]
    fn same_repo_same_id_regardless_of_transport() {
        assert_eq!(
            repo_id("https://github.com/o/r.git"),
            repo_id("git@github.com:o/r")
        );
    }

    #[test]
    fn different_repos_differ() {
        assert_ne!(
            repo_id("https://github.com/o/r1"),
            repo_id("https://github.com/o/r2")
        );
    }

    #[test]
    fn default_ports_dropped() {
        assert_eq!(
            canonical_identity("https://github.com:443/o/r"),
            canonical_identity("https://github.com/o/r")
        );
    }

    #[test]
    fn id_is_filesystem_safe() {
        let id = repo_id("https://github.com/example/huge-repo.git");
        assert!(id.0.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
