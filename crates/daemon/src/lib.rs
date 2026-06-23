//! `glm-daemon` — per-user mount registry, lifecycle, and control logic
//! (spec §39).
//!
//! This crate currently provides the **in-process** control surface used by the
//! CLI: a crash-safe [`Registry`] of [`MountSpec`]s and a [`Controller`] that
//! clones, opens, and resolves mounts. A long-lived socketed daemon process
//! (Unix domain socket / named pipe) exposing the same operations over a
//! versioned protocol is the next step (spec §39, Milestone 5); the transport is
//! intentionally separated from the logic here so it can be added without
//! changing call sites.

#![forbid(unsafe_code)]

mod controller;
mod registry;

pub use controller::{CloneOptions, Controller, OpenMount};
pub use registry::{MountSpec, MountState, Registry};

#[cfg(test)]
mod tests {
    use super::*;
    use glm_core::FetchPolicy;
    use glm_platform::DataRoots;

    #[test]
    fn clone_open_and_resolve_roundtrip() {
        let remote = glm_testkit::seed_remote(&[("a.txt", b"hello\n"), ("src/m.rs", b"x\n")]);
        let data = tempfile::tempdir().unwrap();
        let mountpoint = tempfile::tempdir().unwrap();
        let ctl = Controller::new(DataRoots::ephemeral(data.path()));

        let spec = ctl
            .clone_repo(&remote.url, mountpoint.path(), &CloneOptions::default())
            .unwrap();
        assert_eq!(spec.filter.as_deref(), Some("blob:none"));

        // Resolve from inside the mountpoint and open it.
        let resolved = ctl.resolve_mount(None, mountpoint.path()).unwrap();
        assert_eq!(resolved.id, spec.id);

        let mount = ctl.open(&resolved, None).unwrap();
        // The workspace can list the root from Git trees (no checkout).
        let entries = mount
            .workspace
            .list_dir(&glm_core::RepoPath::root(), FetchPolicy::AllowNetwork)
            .unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&b"a.txt".to_vec()));
        assert!(names.contains(&b"src".to_vec()));

        // Listing and unmounting.
        assert_eq!(ctl.list().unwrap().len(), 1);
        assert!(ctl.unmount(mountpoint.path()).unwrap());
        assert_eq!(ctl.list().unwrap().len(), 0);
    }
}
