//! The libfuse-backed kernel mount: a `fuser::Filesystem` adapter over
//! [`FuseOps`] (spec §40). Compiled only with the `fuse` feature, which links
//! libfuse3 — so this is Linux-only, exercised by the manual `linux-mount` CI
//! job and in Docker, never on the default cross-platform matrix.
//!
//! ## Concurrency (important)
//!
//! fuser's read-dispatch loop is **serial**: it reads one kernel request, calls
//! the matching `Filesystem` method, and only then reads the next. Our methods
//! shell out to `git` (lazy hydration, smudge filters). A subprocess forked while
//! the loop is blocked inside a callback inherits any file the *kernel* has open
//! on this very mount; when that subprocess `exec`s, the kernel closes the
//! inherited descriptor, which issues a `FLUSH` back to **us** — but the only
//! thread that can answer it is the blocked dispatch loop. That is a hard
//! deadlock (kernel stack: `fuse_flush` → `__fuse_simple_request`).
//!
//! fuser documents the escape hatch ("the filesystem methods may run concurrent
//! by spawning threads") and makes the `Reply*` handles `Send`. So every
//! potentially-blocking callback **dispatches onto a worker thread** and replies
//! from there, returning immediately so the loop stays free to service that
//! `FLUSH`. [`FuseOps`] is internally `Mutex`-guarded, so concurrent calls are
//! safe.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    BackgroundSession, FileAttr as FuseFileAttr, FileType, Filesystem, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use glm_core::{Error, ErrorCode, Result};
use glm_fs_common::ROOT_INO;
use glm_workspace::EntryKind;

use crate::FuseOps;

/// Attribute/entry cache lifetime handed back to the kernel.
const TTL: Duration = Duration::from_secs(1);

/// Map a `glm_workspace::EntryKind` to a `fuser::FileType`.
fn file_type(kind: EntryKind) -> FileType {
    match kind {
        EntryKind::Dir | EntryKind::Gitlink => FileType::Directory,
        EntryKind::Symlink => FileType::Symlink,
        EntryKind::File { .. } => FileType::RegularFile,
    }
}

/// Map the neutral engine attributes into `fuser::FileAttr`, owned by the calling
/// user (`uid`/`gid` from the request) with stable synthetic times (spec §28).
fn fuse_attr(a: &glm_fs_common::FileAttr, uid: u32, gid: u32) -> FuseFileAttr {
    let kind = file_type(a.kind);
    FuseFileAttr {
        ino: a.ino,
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm: (a.unix_mode & 0o7777) as u16,
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

/// A synthetic directory attribute (for an implied / freshly-`mkdir`ed dir, which
/// the engine materializes once a child is written under it).
fn synthetic_dir(ino: u64, uid: u32, gid: u32) -> FuseFileAttr {
    FuseFileAttr {
        ino,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// Map a structured engine error to the errno the kernel expects (spec §47).
fn errno(e: &Error) -> i32 {
    e.errno()
}

/// A `fuser::Filesystem` translating FUSE callbacks onto [`FuseOps`]. Blocking
/// work is dispatched onto worker threads (see the module docs).
struct Adapter {
    ops: Arc<FuseOps>,
}

impl Adapter {
    fn new(ops: FuseOps) -> Adapter {
        Adapter { ops: Arc::new(ops) }
    }
}

/// Spawn a worker thread for a blocking callback. Keeps the dispatch loop free.
fn dispatch<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::spawn(f);
}

impl Filesystem for Adapter {
    fn lookup(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: ReplyEntry,
    ) {
        let ops = Arc::clone(&self.ops);
        let name = name.as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        dispatch(move || match ops.lookup(parent, &name) {
            Ok(a) => reply.entry(&TTL, &fuse_attr(&a, uid, gid), a.generation),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        self.ops.forget(ino, nlookup);
    }

    fn getattr(&mut self, req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let ops = Arc::clone(&self.ops);
        let (uid, gid) = (req.uid(), req.gid());
        dispatch(move || match ops.getattr(ino) {
            Ok(a) => reply.attr(&TTL, &fuse_attr(&a, uid, gid)),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let ops = Arc::clone(&self.ops);
        dispatch(move || match ops.readlink(ino) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let ops = Arc::clone(&self.ops);
        dispatch(move || match ops.read(ino, offset.max(0) as u64, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let ops = Arc::clone(&self.ops);
        dispatch(move || {
            let entries = match ops.readdir(ino) {
                Ok(e) => e,
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            };
            // "." and ".." first (both point at this dir's inode — adequate for a
            // synthetic FS; the kernel dcache handles real parent navigation),
            // then the engine's entries. `offset` is the next entry index.
            let mut listing: Vec<(u64, FileType, Vec<u8>)> = Vec::with_capacity(entries.len() + 2);
            listing.push((ino, FileType::Directory, b".".to_vec()));
            listing.push((ino, FileType::Directory, b"..".to_vec()));
            for e in entries {
                listing.push((e.ino, file_type(e.attr.kind), e.name));
            }
            for (i, (eino, kind, name)) in
                listing.into_iter().enumerate().skip(offset.max(0) as usize)
            {
                // `add` returns true when the reply buffer is full; the next call
                // resumes at the offset we pass (i + 1).
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

    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let ops = Arc::clone(&self.ops);
        let name = name.as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        let executable = mode & 0o111 != 0;
        dispatch(move || match ops.create(parent, &name, executable) {
            Ok(a) => reply.created(&TTL, &fuse_attr(&a, uid, gid), a.generation, 0, 0),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let ops = Arc::clone(&self.ops);
        let name = name.as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        dispatch(move || {
            // Git has no empty directories; the engine materializes a directory
            // once a child is written. Return a synthetic dir so `mkdir d && … >
            // d/f` works; the inode is stable for the path.
            let parent_path = match ops.inodes().path_of(parent) {
                Some(p) => p,
                None if parent == ROOT_INO => glm_core::RepoPath::root(),
                None => {
                    reply.error(116); // ESTALE
                    return;
                }
            };
            match parent_path.join(&name) {
                Ok(child) => {
                    let (cino, _gen) = ops.inodes().lookup(&child);
                    reply.entry(&TTL, &synthetic_dir(cino, uid, gid), 0);
                }
                Err(_) => reply.error(22), // EINVAL
            }
        });
    }

    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        link_name: &std::ffi::OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let ops = Arc::clone(&self.ops);
        let name = link_name.as_bytes().to_vec();
        let target = target.as_os_str().as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        dispatch(move || match ops.symlink(parent, &name, &target) {
            Ok(a) => reply.entry(&TTL, &fuse_attr(&a, uid, gid), a.generation),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let ops = Arc::clone(&self.ops);
        let data = data.to_vec();
        dispatch(move || match ops.write(ino, offset.max(0) as u64, &data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(errno(&e)),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let ops = Arc::clone(&self.ops);
        let (uid, gid) = (req.uid(), req.gid());
        dispatch(move || {
            if let Some(len) = size {
                if let Err(e) = ops.truncate(ino, len) {
                    reply.error(errno(&e));
                    return;
                }
            }
            if let Some(m) = mode {
                if let Err(e) = ops.set_executable(ino, m & 0o111 != 0) {
                    reply.error(errno(&e));
                    return;
                }
            }
            match ops.getattr(ino) {
                Ok(a) => reply.attr(&TTL, &fuse_attr(&a, uid, gid)),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn unlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: ReplyEmpty,
    ) {
        let ops = Arc::clone(&self.ops);
        let name = name.as_bytes().to_vec();
        dispatch(move || match ops.remove(parent, &name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn rmdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: ReplyEmpty,
    ) {
        let ops = Arc::clone(&self.ops);
        let name = name.as_bytes().to_vec();
        dispatch(move || match ops.remove(parent, &name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        newparent: u64,
        newname: &std::ffi::OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let ops = Arc::clone(&self.ops);
        let name = name.as_bytes().to_vec();
        let newname = newname.as_bytes().to_vec();
        dispatch(
            move || match ops.rename(parent, &name, newparent, &newname) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(&e)),
            },
        );
    }
}

/// Whether `user_allow_other` is enabled in `/etc/fuse.conf`. `AutoUnmount`
/// requires `AllowOther`, which `fusermount3` only permits for root or when this
/// is set. Requesting it unconditionally fails the mount on locked-down hosts
/// (e.g. unprivileged CI runners), so we gate on it.
fn user_allow_other_permitted() -> bool {
    std::fs::read_to_string("/etc/fuse.conf")
        .map(|s| s.lines().any(|l| l.trim() == "user_allow_other"))
        .unwrap_or(false)
}

fn mount_options() -> Vec<MountOption> {
    let mut opts = vec![
        MountOption::FSName("git-lazy-mount".into()),
        MountOption::Subtype("glm".into()),
    ];
    // Auto-unmount when the serving process exits so a crash/kill never leaves a
    // wedged kernel mount — but only when permitted (see above). Where it isn't,
    // the mount still succeeds and `BackgroundMount`'s drop performs the unmount.
    if user_allow_other_permitted() {
        opts.push(MountOption::AllowOther);
        opts.push(MountOption::AutoUnmount);
    }
    opts
}

/// Mount `ops` at `mountpoint` and serve until unmounted (blocking).
pub fn mount(ops: FuseOps, mountpoint: &Path) -> Result<()> {
    fuser::mount2(Adapter::new(ops), mountpoint, &mount_options()).map_err(|e| {
        Error::new(
            ErrorCode::FilesystemBackendUnavailable,
            format!("fuse mount at {} failed: {e}", mountpoint.display()),
        )
        .with_source(e)
    })
}

/// A live background FUSE mount; unmounts on drop or [`BackgroundMount::unmount`].
pub struct BackgroundMount {
    session: BackgroundSession,
}

impl BackgroundMount {
    /// Explicitly unmount (also happens on drop).
    pub fn unmount(self) {
        drop(self.session);
    }
}

/// Mount `ops` at `mountpoint` on a background thread, returning immediately.
pub fn spawn_mount(ops: FuseOps, mountpoint: &Path) -> Result<BackgroundMount> {
    let session =
        fuser::spawn_mount2(Adapter::new(ops), mountpoint, &mount_options()).map_err(|e| {
            Error::new(
                ErrorCode::FilesystemBackendUnavailable,
                format!("fuse mount at {} failed: {e}", mountpoint.display()),
            )
            .with_source(e)
        })?;
    Ok(BackgroundMount { session })
}
