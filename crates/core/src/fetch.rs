//! Object-fetch policy and priority.

use serde::{Deserialize, Serialize};

/// Controls whether resolving an object may touch the network.
///
/// filesystem callbacks must run with `MustNotFetch` so a read
/// never triggers an implicit credential prompt; only the fetch scheduler is
/// authorized to escalate to `AllowNetwork`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FetchPolicy {
    /// Serve only from local caches/objects. Never contact the network.
    /// Maps to `GIT_NO_LAZY_FETCH=1` on the Git side.
    CacheOnly,
    /// May contact the network if the object is missing locally.
    AllowNetwork,
    /// Background/speculative retrieval; lower priority than on-demand.
    Prefetch,
    /// Strictest form of `CacheOnly`: a missing object is a hard error and the
    /// code path asserts it never initiates I/O (used inside fs callbacks).
    MustNotFetch,
}

impl FetchPolicy {
    /// Whether this policy permits initiating a network fetch.
    pub fn may_fetch(&self) -> bool {
        matches!(self, FetchPolicy::AllowNetwork | FetchPolicy::Prefetch)
    }
}

/// Scheduling priority for a fetch request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FetchPriority {
    /// Speculative; may be dropped under pressure.
    Background,
    /// Explicit user prefetch.
    Prefetch,
    /// Blocking an interactive read/operation.
    Interactive,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_only_never_fetches() {
        assert!(!FetchPolicy::CacheOnly.may_fetch());
        assert!(!FetchPolicy::MustNotFetch.may_fetch());
        assert!(FetchPolicy::AllowNetwork.may_fetch());
    }

    #[test]
    fn priority_order() {
        assert!(FetchPriority::Background < FetchPriority::Interactive);
    }
}
