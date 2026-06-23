//! Runtime backend capability detection + installation diagnostics (issue #6,
//! spec §41).
//!
//! [`Capability::detect`] *probes* the running system for a usable macOS
//! filesystem backend instead of assuming one is (or is not) present. It is the
//! seam the CLI uses to decide between a real kernel mount and the headless
//! fallback, and the source of the actionable diagnostics shown when no backend
//! is available (what is missing, and how to install / approve it).
//!
//! The probe is deliberately read-only and non-interactive: it never prompts,
//! never mutates system state, and degrades to an honest "not available" with a
//! concrete next step rather than a bare `false`.

use serde::Serialize;

/// The Apple-issued team/bundle identity the FSKit file-system module ships
/// under. Used to recognize our own system extension in `systemextensionsctl`
/// output. (The signed extension itself is delivered on-device; see issue #10.)
pub const FSKIT_MODULE_BUNDLE_ID: &str = "com.git-lazy-mount.fskit.fsmodule";

/// Minimum macOS major version that exposes third-party FSKit `FSModule`s.
/// Third-party FSKit modules became available in macOS 15.4 (Sequoia); any
/// later train (including the 26.x "Tahoe" line) qualifies.
const FSKIT_MIN_MAJOR: u32 = 15;
const FSKIT_MIN_MINOR_AT_MIN_MAJOR: u32 = 4;

/// Which macOS filesystem backend would serve a mount. The two are **distinct
/// backend boundaries** with identical engine semantics (spec §41): FSKit is
/// preferred; macFUSE is offered only as an explicit, separate fallback and is
/// never selected by silently changing behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MacBackend {
    /// Apple FSKit `FSUnaryFileSystem` / `FSVolume` system extension.
    Fskit,
    /// Isolated macFUSE backend (older systems lacking usable FSKit).
    MacFuse,
}

impl MacBackend {
    /// A stable, human-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            MacBackend::Fskit => "fskit",
            MacBackend::MacFuse => "macfuse",
        }
    }
}

/// A probed snapshot of the host's macOS filesystem-backend capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Capability {
    /// Whether this host is macOS at all.
    pub platform_is_macos: bool,
    /// The detected macOS product version (e.g. `"26.4.1"`), if known.
    pub os_version: Option<String>,
    /// macOS is new enough to host a third-party FSKit module (>= 15.4).
    pub fskit_os_supported: bool,
    /// Our FSKit file-system module (system extension) is installed.
    pub fskit_module_installed: bool,
    /// The installed FSKit system extension is user-approved / activated.
    pub fskit_module_approved: bool,
    /// A macFUSE installation is present.
    pub macfuse_installed: bool,
}

impl Capability {
    /// Probe the running system. Read-only and non-interactive.
    pub fn detect() -> Capability {
        #[cfg(target_os = "macos")]
        {
            let os_version = macos::product_version();
            let fskit_os_supported = os_version
                .as_deref()
                .map(macos::version_supports_fskit)
                .unwrap_or(false);
            let (fskit_module_installed, fskit_module_approved) = macos::fskit_module_status();
            Capability {
                platform_is_macos: true,
                os_version,
                fskit_os_supported,
                fskit_module_installed,
                fskit_module_approved,
                macfuse_installed: macos::macfuse_installed(),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Capability {
                platform_is_macos: false,
                os_version: None,
                fskit_os_supported: false,
                fskit_module_installed: false,
                fskit_module_approved: false,
                macfuse_installed: false,
            }
        }
    }

    /// The backend that would actually be used, if any. FSKit wins when the OS
    /// supports it and our extension is installed **and** approved; otherwise an
    /// installed macFUSE is used as the explicit fallback.
    pub fn selected_backend(&self) -> Option<MacBackend> {
        if self.fskit_os_supported && self.fskit_module_installed && self.fskit_module_approved {
            Some(MacBackend::Fskit)
        } else if self.macfuse_installed {
            Some(MacBackend::MacFuse)
        } else {
            None
        }
    }

    /// Whether a usable backend is present.
    pub fn is_usable(&self) -> bool {
        self.selected_backend().is_some()
    }

    /// Concrete, ordered steps to obtain a usable backend. Empty when one is
    /// already available. These are surfaced verbatim by `mount()` failures and
    /// `git lazy-mount doctor` so the user is never left with "not implemented".
    pub fn diagnostics(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.platform_is_macos {
            out.push(
                "the FSKit backend only runs on macOS; on this host use the platform's own \
                 backend or the headless CLI"
                    .to_string(),
            );
            return out;
        }
        if self.is_usable() {
            return out;
        }
        if !self.fskit_os_supported {
            out.push(format!(
                "macOS {} predates third-party FSKit (requires macOS {FSKIT_MIN_MAJOR}.{FSKIT_MIN_MINOR_AT_MIN_MAJOR}+); \
                 upgrade macOS for FSKit, or install macFUSE as a fallback",
                self.os_version.as_deref().unwrap_or("(unknown)")
            ));
        } else if !self.fskit_module_installed {
            out.push(
                "the git-lazy-mount FSKit file-system module is not installed; install the \
                 app bundle and enable its system extension under System Settings → General → \
                 Login Items & Extensions → File System Extensions"
                    .to_string(),
            );
        } else if !self.fskit_module_approved {
            out.push(
                "the git-lazy-mount FSKit system extension is installed but awaiting approval; \
                 approve it under System Settings → General → Login Items & Extensions, then retry"
                    .to_string(),
            );
        }
        if !self.macfuse_installed {
            out.push(
                "alternatively install macFUSE (https://macfuse.io) to use the isolated macFUSE \
                 backend"
                    .to_string(),
            );
        }
        out.push(
            "until a backend is available, use the headless CLI (ls / cat / status / commit …), \
             which drives the same workspace engine"
                .to_string(),
        );
        out
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::Path;
    use std::process::Command;

    use super::FSKIT_MODULE_BUNDLE_ID;

    /// `sw_vers -productVersion`, e.g. `"26.4.1"`.
    pub fn product_version() -> Option<String> {
        let out = Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    /// Whether `version` (dotted, e.g. `"15.4"` / `"26.4.1"`) is >= 15.4.
    pub fn version_supports_fskit(version: &str) -> bool {
        let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        let major = parts.next().unwrap_or(0);
        let minor = parts.next().unwrap_or(0);
        match major.cmp(&super::FSKIT_MIN_MAJOR) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => minor >= super::FSKIT_MIN_MINOR_AT_MIN_MAJOR,
        }
    }

    /// `(installed, approved)` for our FSKit system extension, read from
    /// `systemextensionsctl list`. Absent tooling or extension yields
    /// `(false, false)` — the honest "nothing installed" answer.
    pub fn fskit_module_status() -> (bool, bool) {
        let Ok(out) = Command::new("/usr/bin/systemextensionsctl")
            .arg("list")
            .output()
        else {
            return (false, false);
        };
        if !out.status.success() {
            return (false, false);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().find(|l| l.contains(FSKIT_MODULE_BUNDLE_ID));
        match line {
            None => (false, false),
            // `systemextensionsctl list` marks an active, approved extension with
            // the `[activated enabled]` state.
            Some(l) => (true, l.contains("activated enabled")),
        }
    }

    /// Whether a macFUSE (or legacy osxfuse) file system is installed.
    pub fn macfuse_installed() -> bool {
        Path::new("/Library/Filesystems/macfuse.fs").exists()
            || Path::new("/Library/Filesystems/osxfuse.fs").exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_is_consistent_with_selection() {
        let cap = Capability::detect();
        // `is_usable` and `selected_backend` must never disagree.
        assert_eq!(cap.is_usable(), cap.selected_backend().is_some());
        // A usable backend means no diagnostics; an unusable one always offers a
        // concrete next step (never an empty dead-end).
        if cap.is_usable() {
            assert!(cap.diagnostics().is_empty());
        } else {
            assert!(!cap.diagnostics().is_empty());
        }
    }

    #[test]
    fn non_macos_reports_not_macos() {
        // Construct the non-macOS shape explicitly so the assertion holds on
        // every CI runner regardless of host.
        let cap = Capability {
            platform_is_macos: false,
            os_version: None,
            fskit_os_supported: false,
            fskit_module_installed: false,
            fskit_module_approved: false,
            macfuse_installed: false,
        };
        assert!(!cap.is_usable());
        assert!(cap.diagnostics()[0].contains("only runs on macOS"));
    }

    #[test]
    fn fskit_selected_only_when_installed_and_approved() {
        let base = Capability {
            platform_is_macos: true,
            os_version: Some("26.4.1".into()),
            fskit_os_supported: true,
            fskit_module_installed: true,
            fskit_module_approved: false,
            macfuse_installed: false,
        };
        // Installed but unapproved -> not usable; diagnostic points at approval.
        assert_eq!(base.selected_backend(), None);
        assert!(base
            .diagnostics()
            .iter()
            .any(|d| d.contains("awaiting approval")));

        let approved = Capability {
            fskit_module_approved: true,
            ..base.clone()
        };
        assert_eq!(approved.selected_backend(), Some(MacBackend::Fskit));
    }

    #[test]
    fn macfuse_is_the_explicit_fallback() {
        let cap = Capability {
            platform_is_macos: true,
            os_version: Some("13.0".into()),
            fskit_os_supported: false,
            fskit_module_installed: false,
            fskit_module_approved: false,
            macfuse_installed: true,
        };
        // Old OS, no FSKit, but macFUSE present -> the fallback boundary is used.
        assert_eq!(cap.selected_backend(), Some(MacBackend::MacFuse));
        assert!(cap.is_usable());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_version_threshold() {
        use super::macos::version_supports_fskit;
        assert!(!version_supports_fskit("15.3"));
        assert!(version_supports_fskit("15.4"));
        assert!(version_supports_fskit("15.5"));
        assert!(version_supports_fskit("26.4.1"));
        assert!(!version_supports_fskit("14.7.2"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_host_reports_a_version() {
        // On a real macOS host the probe must read a product version.
        let cap = Capability::detect();
        assert!(cap.platform_is_macos);
        assert!(cap.os_version.is_some(), "sw_vers should report a version");
    }
}
