//! FSKit system-extension lifecycle (issue #10, spec §41).
//!
//! An FSKit module ships as a **system extension** inside an app bundle. Before
//! it can mount anything it must be installed and then *approved* by the user;
//! macOS may also require re-approval after an OS update. This module turns a
//! [`Capability`] probe into an explicit [`ExtensionState`] and the concrete next
//! step, so the lifecycle is surfaced through diagnostics rather than guessed at.
//!
//! The signing requirements, the entitlements the extension needs, and the
//! reproducible signed-build steps live alongside this crate under
//! `extension/` (`Info.plist`, `git-lazy-mount.entitlements`, `README.md`).
//! Actually producing a signed build and exercising the activation/approval flow
//! requires an Apple Developer identity on real hardware (tracked by issue #12).

use crate::capability::Capability;

/// Where the FSKit system extension is in its install/approval lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionState {
    /// Not macOS, or macOS too old for third-party FSKit.
    Unsupported,
    /// Supported OS, but our system extension is not installed.
    NotInstalled,
    /// Installed, waiting for the user to approve it in System Settings.
    AwaitingApproval,
    /// Installed and approved/activated; ready to mount.
    Activated,
}

impl ExtensionState {
    /// A stable label for diagnostics / JSON.
    pub fn label(&self) -> &'static str {
        match self {
            ExtensionState::Unsupported => "unsupported",
            ExtensionState::NotInstalled => "not_installed",
            ExtensionState::AwaitingApproval => "awaiting_approval",
            ExtensionState::Activated => "activated",
        }
    }

    /// The concrete next step to reach [`Activated`], or `None` when already
    /// activated (or unsupported, where the next step is OS-level).
    ///
    /// [`Activated`]: ExtensionState::Activated
    pub fn next_step(&self) -> Option<&'static str> {
        match self {
            ExtensionState::Unsupported => {
                Some("upgrade to macOS 15.4+ for FSKit, or install macFUSE as a fallback")
            }
            ExtensionState::NotInstalled => {
                Some("install the git-lazy-mount app bundle to register its FSKit system extension")
            }
            ExtensionState::AwaitingApproval => Some(
                "approve the extension in System Settings → General → Login Items & Extensions \
                 → File System Extensions",
            ),
            ExtensionState::Activated => None,
        }
    }
}

/// Derive the [`ExtensionState`] from a capability probe.
pub fn extension_state(cap: &Capability) -> ExtensionState {
    if !cap.platform_is_macos || !cap.fskit_os_supported {
        ExtensionState::Unsupported
    } else if !cap.fskit_module_installed {
        ExtensionState::NotInstalled
    } else if !cap.fskit_module_approved {
        ExtensionState::AwaitingApproval
    } else {
        ExtensionState::Activated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(os: bool, installed: bool, approved: bool) -> Capability {
        Capability {
            platform_is_macos: true,
            os_version: Some("26.4.1".into()),
            fskit_os_supported: os,
            fskit_module_installed: installed,
            fskit_module_approved: approved,
            macfuse_installed: false,
        }
    }

    #[test]
    fn lifecycle_progression() {
        assert_eq!(
            extension_state(&cap(false, false, false)),
            ExtensionState::Unsupported
        );
        assert_eq!(
            extension_state(&cap(true, false, false)),
            ExtensionState::NotInstalled
        );
        assert_eq!(
            extension_state(&cap(true, true, false)),
            ExtensionState::AwaitingApproval
        );
        assert_eq!(
            extension_state(&cap(true, true, true)),
            ExtensionState::Activated
        );
    }

    #[test]
    fn only_activated_has_no_next_step() {
        for (state, has_step) in [
            (ExtensionState::Unsupported, true),
            (ExtensionState::NotInstalled, true),
            (ExtensionState::AwaitingApproval, true),
            (ExtensionState::Activated, false),
        ] {
            assert_eq!(state.next_step().is_some(), has_step, "{state:?}");
        }
    }
}
