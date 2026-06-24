//! The read-only `fuser::Filesystem` over [`glm_worktree::Projection`], with
//! real file handles and a bounded worker pool (see the crate docs).

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    BackgroundSession, FileAttr as FuseFileAttr, FileType, Filesystem, MountOption, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};
use glm_core::{Error, ErrorCode, Result};
use glm_worktree::{Attr, ContentHandle, Kind, Projection};

use crate::pool::Pool;

/// Attribute/entry cache lifetime handed to the kernel.
const TTL: Duration = Duration::from_secs(1);
/// Worker threads for blocking callbacks (object IO). Bounded — see crate docs.
const POOL_THREADS: usize = 16;

fn file_type(kind: Kind) -> FileType {
    match kind {
        Kind::Dir => FileType::Directory,
        Kind::Symlink => FileType::Symlink,
        Kind::File { .. } => FileType::RegularFile,
    }
}

/// Map neutral projection attributes to `fuser::FileAttr`. The projection is
/// read-only here, so perms carry no write bit (the writable overlay arrives in
/// M2). Times are stable synthetic values (redesign.md §22).
fn fuse_attr(a: &Attr, uid: u32, gid: u32) -> FuseFileAttr {
    let (kind, perm) = match a.kind {
        Kind::Dir => (FileType::Directory, 0o555),
        Kind::Symlink => (FileType::Symlink, 0o777),
        Kind::File { executable: true } => (FileType::RegularFile, 0o555),
        Kind::File { executable: false } => (FileType::RegularFile, 0o444),
    };
    FuseFileAttr {
        ino: a.ino,
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink: if matches!(kind, FileType::Directory) {
            2
        } else {
            1
        },
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

fn errno(e: &Error) -> i32 {
    e.errno()
}

/// A read-only transparent mount. Holds the projection, a real handle table, and
/// the bounded worker pool.
struct ReadOnlyFs {
    proj: Arc<Projection>,
    handles: Arc<Mutex<HashMap<u64, Arc<ContentHandle>>>>,
    next_fh: Arc<AtomicU64>,
    pool: Pool,
}

impl ReadOnlyFs {
    fn new(proj: Arc<Projection>) -> ReadOnlyFs {
        ReadOnlyFs {
            proj,
            handles: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(AtomicU64::new(1)),
            pool: Pool::new(POOL_THREADS),
        }
    }
}

impl Filesystem for ReadOnlyFs {
    fn lookup(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: ReplyEntry,
    ) {
        let proj = Arc::clone(&self.proj);
        let name = name.as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        self.pool.spawn(move || match proj.lookup(parent, &name) {
            Ok(Some(a)) => reply.entry(&TTL, &fuse_attr(&a, uid, gid), a.generation),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        self.proj.forget(ino, nlookup);
    }

    fn getattr(&mut self, req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let proj = Arc::clone(&self.proj);
        let (uid, gid) = (req.uid(), req.gid());
        self.pool.spawn(move || match proj.getattr(ino) {
            Ok(a) => reply.attr(&TTL, &fuse_attr(&a, uid, gid)),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let proj = Arc::clone(&self.proj);
        self.pool.spawn(move || match proj.readlink(ino) {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        // Read-only projection: reject any write intent (§ the overlay is M2).
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            reply.error(libc::EROFS);
            return;
        }
        let proj = Arc::clone(&self.proj);
        let handles = Arc::clone(&self.handles);
        let next_fh = Arc::clone(&self.next_fh);
        self.pool.spawn(move || match proj.open_content(ino) {
            Ok(h) => {
                let fh = next_fh.fetch_add(1, Ordering::Relaxed);
                handles.lock().unwrap().insert(fh, Arc::new(h));
                reply.opened(fh, 0);
            }
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // Service the read from the real handle — never a path re-resolution.
        let handle = self.handles.lock().unwrap().get(&fh).cloned();
        let Some(handle) = handle else {
            reply.error(libc::EBADF);
            return;
        };
        let off = offset.max(0) as u64;
        let len = size as usize;
        self.pool.spawn(move || match handle.read_at(off, len) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.lock().unwrap().remove(&fh);
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let proj = Arc::clone(&self.proj);
        self.pool.spawn(move || {
            let entries = match proj.readdir(ino) {
                Ok(e) => e,
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            };
            // "." and ".." then the projected entries; readdir carries names +
            // d_type only — no sizes, no blob reads (§4.5).
            let mut listing: Vec<(u64, FileType, Vec<u8>)> = Vec::with_capacity(entries.len() + 2);
            listing.push((ino, FileType::Directory, b".".to_vec()));
            listing.push((ino, FileType::Directory, b"..".to_vec()));
            for e in entries {
                listing.push((e.ino, file_type(e.kind), e.name));
            }
            for (i, (eino, kind, name)) in
                listing.into_iter().enumerate().skip(offset.max(0) as usize)
            {
                if reply.add(
                    eino,
                    (i + 1) as i64,
                    kind,
                    std::ffi::OsStr::from_bytes(&name),
                ) {
                    break;
                }
            }
            reply.ok();
        });
    }

    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // Synthetic, non-zero values so tools don't treat the FS as full.
        reply.statfs(1 << 30, 1 << 30, 1 << 30, 1 << 20, 1 << 20, 512, 255, 512);
    }
}

/// Whether `user_allow_other` is set in `/etc/fuse.conf` (`AutoUnmount` needs
/// `AllowOther`, only permitted then for unprivileged users).
fn user_allow_other_permitted() -> bool {
    std::fs::read_to_string("/etc/fuse.conf")
        .map(|s| s.lines().any(|l| l.trim() == "user_allow_other"))
        .unwrap_or(false)
}

fn mount_options() -> Vec<MountOption> {
    let mut opts = vec![
        MountOption::FSName("git-lazy-mount".into()),
        MountOption::Subtype("glm".into()),
        MountOption::RO,
        MountOption::DefaultPermissions,
    ];
    if user_allow_other_permitted() {
        opts.push(MountOption::AllowOther);
        opts.push(MountOption::AutoUnmount);
    }
    opts
}

/// A live background mount; unmounts on drop or [`BackgroundMount::unmount`].
pub struct BackgroundMount {
    session: BackgroundSession,
}

impl BackgroundMount {
    /// Explicitly unmount (also happens on drop).
    pub fn unmount(self) {
        drop(self.session);
    }
}

/// Mount `proj` read-only at `mountpoint` and serve until unmounted (blocking).
pub fn mount(proj: Arc<Projection>, mountpoint: &Path) -> Result<()> {
    fuser::mount2(ReadOnlyFs::new(proj), mountpoint, &mount_options()).map_err(|e| {
        Error::new(
            ErrorCode::FilesystemBackendUnavailable,
            format!("fuse mount at {} failed: {e}", mountpoint.display()),
        )
        .with_source(e)
    })
}

/// Mount `proj` read-only on a background thread, returning immediately.
pub fn spawn_mount(proj: Arc<Projection>, mountpoint: &Path) -> Result<BackgroundMount> {
    let session = fuser::spawn_mount2(ReadOnlyFs::new(proj), mountpoint, &mount_options())
        .map_err(|e| {
            Error::new(
                ErrorCode::FilesystemBackendUnavailable,
                format!("fuse mount at {} failed: {e}", mountpoint.display()),
            )
            .with_source(e)
        })?;
    Ok(BackgroundMount { session })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glm_git_repo::{AdminRepo, CloneOptions};
    use std::process::Command;

    fn git(args: &[&str]) -> (bool, String) {
        let out = Command::new("git").args(args).output().expect("spawn git");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        )
    }

    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..500 {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn experiment_a_b_transparent_readonly_mount() {
        let remote = glm_testkit::seed_remote(&[
            ("README.md", b"hello world\n"),
            ("src/main.rs", b"fn main() {}\n"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let mnt = tmp.path().join("mnt");
        let repo = AdminRepo::clone(
            &remote.url,
            &tmp.path().join("git"),
            &mnt,
            &tmp.path().join("anchor"),
            &CloneOptions::default(),
        )
        .unwrap();
        let gitdir = repo.gitdir().to_path_buf();
        let proj = Arc::new(
            Projection::open(repo, tmp.path().join("cache"), tmp.path().join("overlay")).unwrap(),
        );
        let mount = spawn_mount(Arc::clone(&proj), &mnt).unwrap();
        assert!(
            wait_until(|| mnt.join(".git").exists()),
            "mount did not become ready"
        );

        let mnt_s = mnt.to_str().unwrap();

        // Experiment A: stock git resolves the repo through the synthetic .git.
        let (ok, top) = git(&["-C", mnt_s, "rev-parse", "--show-toplevel"]);
        assert!(ok, "rev-parse failed");
        assert_eq!(
            Path::new(&top).canonicalize().unwrap(),
            mnt.canonicalize().unwrap(),
            "show-toplevel is the mountpoint"
        );
        assert_eq!(
            git(&["-C", mnt_s, "rev-parse", "--is-inside-work-tree"]).1,
            "true"
        );
        // The synthetic .git gitfile content points at the admin gitdir.
        let gitfile = std::fs::read_to_string(mnt.join(".git")).unwrap();
        assert_eq!(gitfile.trim(), format!("gitdir: {}", gitdir.display()));

        // Experiment B: a plain directory listing hydrates ZERO blobs (§38.2).
        let before = proj.hydrations();
        let mut names: Vec<String> = std::fs::read_dir(&mnt)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert!(names.contains(&"README.md".to_string()));
        assert!(names.contains(&"src".to_string()));
        assert!(names.contains(&".git".to_string()));
        assert_eq!(proj.hydrations(), before, "readdir hydrated a blob");

        // Reading one file hydrates exactly that blob (§38.5), with correct bytes.
        assert_eq!(
            std::fs::read_to_string(mnt.join("README.md")).unwrap(),
            "hello world\n"
        );
        assert_eq!(proj.hydrations(), before + 1, "cat hydrated one blob");
        assert_eq!(
            std::fs::read_to_string(mnt.join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );

        mount.unmount();
    }
}
