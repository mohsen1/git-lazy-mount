//! The transparent `fuser::Filesystem` over [`glm_worktree::Projection`], with
//! real file handles, a bounded worker pool, and the writable overlay (M2).

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    BackgroundSession, FileAttr as FuseFileAttr, FileType, Filesystem, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow,
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

/// Map neutral projection attributes to `fuser::FileAttr` with writable perms
/// (the overlay backs writes; the synthetic `.git` is protected by the
/// projection, which rejects mutations regardless of perms — §6). Times are
/// stable synthetic values (design.md §22).
fn fuse_attr(a: &Attr, uid: u32, gid: u32) -> FuseFileAttr {
    let (kind, perm) = match a.kind {
        Kind::Dir => (FileType::Directory, 0o755),
        Kind::Symlink => (FileType::Symlink, 0o777),
        Kind::File { executable: true } => (FileType::RegularFile, 0o755),
        Kind::File { executable: false } => (FileType::RegularFile, 0o644),
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

/// An open handle: a read stream, or a writable overlay FD (also readable). Each
/// carries its inode so `getattr` can serve a deleted-but-open file from the live
/// fd after `unlink` (§17.4).
enum Handle {
    Read {
        ino: u64,
        content: Arc<ContentHandle>,
    },
    Write {
        ino: u64,
        file: Arc<std::fs::File>,
    },
}

impl Handle {
    fn ino(&self) -> u64 {
        match self {
            Handle::Read { ino, .. } | Handle::Write { ino, .. } => *ino,
        }
    }
}

/// The transparent mount: the projection, a real handle table, the worker pool.
struct TransparentFs {
    proj: Arc<Projection>,
    handles: Arc<Mutex<HashMap<u64, Handle>>>,
    next_fh: Arc<AtomicU64>,
    pool: Pool,
}

impl TransparentFs {
    fn new(proj: Arc<Projection>) -> TransparentFs {
        TransparentFs {
            proj,
            handles: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(AtomicU64::new(1)),
            pool: Pool::new(POOL_THREADS),
        }
    }
}

impl Filesystem for TransparentFs {
    fn init(
        &mut self,
        _req: &Request<'_>,
        config: &mut fuser::KernelConfig,
    ) -> std::result::Result<(), libc::c_int> {
        // Handle `O_TRUNC` atomically in `open` so a truncating open is delivered
        // as one `open(O_TRUNC)` call instead of `open`(no-trunc)+`setattr(0)` —
        // the latter would copy the old blob up before truncating it (§38.7).
        // `FUSE_ATOMIC_O_TRUNC = 1 << 3`; fall back silently if unsupported.
        let _ = config.add_capabilities(1 << 3);
        Ok(())
    }

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
        let handles = Arc::clone(&self.handles);
        let (uid, gid) = (req.uid(), req.gid());
        self.pool.spawn(move || match proj.getattr(ino) {
            Ok(a) => reply.attr(&TTL, &fuse_attr(&a, uid, gid)),
            Err(e) => {
                // Deleted-but-open fallback (§17.4): if the path is gone but an
                // fd is still open on this inode, serve a regular-file attr sized
                // from that fd so `seek(End)`/`fstat` keep working.
                if let Some(size) = open_size(&handles, ino) {
                    let a = Attr {
                        ino,
                        generation: 0,
                        size,
                        kind: Kind::File { executable: false },
                    };
                    reply.attr(&TTL, &fuse_attr(&a, uid, gid));
                } else {
                    reply.error(errno(&e));
                }
            }
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
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let proj = Arc::clone(&self.proj);
        let (uid, gid) = (req.uid(), req.gid());
        self.pool.spawn(move || {
            if let Some(len) = size {
                if let Err(e) = proj.truncate(ino, len) {
                    reply.error(errno(&e));
                    return;
                }
            }
            if let Some(m) = mode {
                if let Err(e) = proj.set_executable(ino, m & 0o111 != 0) {
                    reply.error(errno(&e));
                    return;
                }
            }
            match proj.getattr(ino) {
                Ok(a) => reply.attr(&TTL, &fuse_attr(&a, uid, gid)),
                Err(e) => reply.error(errno(&e)),
            }
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
        let proj = Arc::clone(&self.proj);
        let handles = Arc::clone(&self.handles);
        let next_fh = Arc::clone(&self.next_fh);
        let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
        let truncate = flags & libc::O_TRUNC != 0;
        self.pool.spawn(move || {
            let h = if writable {
                proj.open_write(ino, truncate).map(|f| Handle::Write {
                    ino,
                    file: Arc::new(f),
                })
            } else {
                proj.open_content(ino).map(|c| Handle::Read {
                    ino,
                    content: Arc::new(c),
                })
            };
            match h {
                Ok(handle) => {
                    let fh = next_fh.fetch_add(1, Ordering::Relaxed);
                    handles.lock().unwrap().insert(fh, handle);
                    reply.opened(fh, 0);
                }
                Err(e) => reply.error(errno(&e)),
            }
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
        let proj = Arc::clone(&self.proj);
        let handles = Arc::clone(&self.handles);
        let next_fh = Arc::clone(&self.next_fh);
        let name = name.as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        let executable = mode & 0o111 != 0;
        self.pool
            .spawn(move || match proj.create(parent, &name, executable) {
                Ok((a, file)) => {
                    let fh = next_fh.fetch_add(1, Ordering::Relaxed);
                    handles.lock().unwrap().insert(
                        fh,
                        Handle::Write {
                            ino: a.ino,
                            file: Arc::new(file),
                        },
                    );
                    reply.created(&TTL, &fuse_attr(&a, uid, gid), a.generation, fh, 0);
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
        let handle = self.lookup_handle(fh);
        let off = offset.max(0) as u64;
        let len = size as usize;
        self.pool.spawn(move || match handle {
            Some(Handle::Read { content, .. }) => match content.read_at(off, len) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => reply.error(errno(&e)),
            },
            Some(Handle::Write { file, .. }) => {
                let mut buf = vec![0u8; len];
                match file.read_at(&mut buf, off) {
                    Ok(n) => {
                        buf.truncate(n);
                        reply.data(&buf);
                    }
                    Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            None => reply.error(libc::EBADF),
        });
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let handle = self.lookup_handle(fh);
        let off = offset.max(0) as u64;
        let data = data.to_vec();
        self.pool.spawn(move || match handle {
            Some(Handle::Write { file, .. }) => match file.write_at(&data, off) {
                Ok(n) => reply.written(n as u32),
                Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
            },
            Some(Handle::Read { .. }) => reply.error(libc::EBADF),
            None => reply.error(libc::EBADF),
        });
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        // Writes go through to the overlay FD; no per-handle buffer to flush.
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let handle = self.lookup_handle(fh);
        self.pool.spawn(move || match handle {
            Some(Handle::Write { file, .. }) => {
                let r = if datasync {
                    file.sync_data()
                } else {
                    file.sync_all()
                };
                match r {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            _ => reply.ok(),
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

    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let proj = Arc::clone(&self.proj);
        let name = name.as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        self.pool.spawn(move || match proj.mkdir(parent, &name) {
            Ok(a) => reply.entry(&TTL, &fuse_attr(&a, uid, gid), a.generation),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn unlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: ReplyEmpty,
    ) {
        let proj = Arc::clone(&self.proj);
        let name = name.as_bytes().to_vec();
        self.pool.spawn(move || match proj.unlink(parent, &name) {
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
        let proj = Arc::clone(&self.proj);
        let name = name.as_bytes().to_vec();
        self.pool.spawn(move || match proj.rmdir(parent, &name) {
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
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let proj = Arc::clone(&self.proj);
        let name = name.as_bytes().to_vec();
        let newname = newname.as_bytes().to_vec();
        self.pool.spawn(
            move || match proj.rename(parent, &name, newparent, &newname, flags) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(&e)),
            },
        );
    }

    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        link_name: &std::ffi::OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let proj = Arc::clone(&self.proj);
        let name = link_name.as_bytes().to_vec();
        let target = target.as_os_str().as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        self.pool
            .spawn(move || match proj.symlink(parent, &name, &target) {
                Ok(a) => reply.entry(&TTL, &fuse_attr(&a, uid, gid), a.generation),
                Err(e) => reply.error(errno(&e)),
            });
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
        reply.statfs(1 << 30, 1 << 30, 1 << 30, 1 << 20, 1 << 20, 512, 255, 512);
    }
}

impl TransparentFs {
    fn lookup_handle(&self, fh: u64) -> Option<Handle> {
        self.handles.lock().unwrap().get(&fh).map(|h| match h {
            Handle::Read { ino, content } => Handle::Read {
                ino: *ino,
                content: Arc::clone(content),
            },
            Handle::Write { ino, file } => Handle::Write {
                ino: *ino,
                file: Arc::clone(file),
            },
        })
    }
}

/// Size of an open handle on `ino`, if any — used to serve `getattr` for a
/// deleted-but-open inode after `unlink` (§17.4). Prefers a writable fd (its
/// size reflects writes); falls back to a read handle's content size.
fn open_size(handles: &Mutex<HashMap<u64, Handle>>, ino: u64) -> Option<u64> {
    let handles = handles
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut size = None;
    for h in handles.values() {
        if h.ino() != ino {
            continue;
        }
        match h {
            Handle::Write { file, .. } => return file.metadata().ok().map(|m| m.len()),
            Handle::Read { content, .. } => size = content.size().ok(),
        }
    }
    size
}

/// Whether `user_allow_other` is set in `/etc/fuse.conf`.
fn user_allow_other_permitted() -> bool {
    std::fs::read_to_string("/etc/fuse.conf")
        .map(|s| s.lines().any(|l| l.trim() == "user_allow_other"))
        .unwrap_or(false)
}

fn mount_options() -> Vec<MountOption> {
    let mut opts = vec![
        MountOption::FSName("git-lazy-mount".into()),
        MountOption::Subtype("glm".into()),
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

/// Mount `proj` at `mountpoint` and serve until unmounted (blocking).
pub fn mount(proj: Arc<Projection>, mountpoint: &Path) -> Result<()> {
    fuser::mount2(TransparentFs::new(proj), mountpoint, &mount_options()).map_err(|e| {
        Error::new(
            ErrorCode::FilesystemBackendUnavailable,
            format!("fuse mount at {} failed: {e}", mountpoint.display()),
        )
        .with_source(e)
    })
}

/// Mount `proj` on a background thread, returning immediately.
pub fn spawn_mount(proj: Arc<Projection>, mountpoint: &Path) -> Result<BackgroundMount> {
    let session = fuser::spawn_mount2(TransparentFs::new(proj), mountpoint, &mount_options())
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
    fn experiment_a_b_c_transparent_mount() {
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
        assert!(wait_until(|| mnt.join(".git").exists()), "mount not ready");
        let mnt_s = mnt.to_str().unwrap();

        // Experiment A: stock git resolves the repo through the synthetic .git.
        let (ok, top) = git(&["-C", mnt_s, "rev-parse", "--show-toplevel"]);
        assert!(ok);
        assert_eq!(
            Path::new(&top).canonicalize().unwrap(),
            mnt.canonicalize().unwrap()
        );
        let gitfile = std::fs::read_to_string(mnt.join(".git")).unwrap();
        assert_eq!(gitfile.trim(), format!("gitdir: {}", gitdir.display()));

        // Experiment B: ls hydrates zero blobs; one read hydrates one.
        let before = proj.hydrations();
        let mut names: Vec<String> = std::fs::read_dir(&mnt)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert!(names.contains(&"README.md".to_string()));
        assert!(names.contains(&".git".to_string()));
        assert_eq!(proj.hydrations(), before, "readdir hydrated a blob");
        assert_eq!(
            std::fs::read_to_string(mnt.join("README.md")).unwrap(),
            "hello world\n"
        );

        // Experiment C: writes through the mount land in the overlay.
        // (1) create a new file
        std::fs::write(mnt.join("new.txt"), b"created\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(mnt.join("new.txt")).unwrap(),
            "created\n"
        );
        // (2) edit an existing (baseline) file via truncate+write (atomic-ish)
        std::fs::write(mnt.join("README.md"), b"edited!\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(mnt.join("README.md")).unwrap(),
            "edited!\n"
        );
        // (3) append to a file
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(mnt.join("new.txt"))
                .unwrap();
            f.write_all(b"more\n").unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(mnt.join("new.txt")).unwrap(),
            "created\nmore\n"
        );
        // (4) mkdir + create under it
        std::fs::create_dir(mnt.join("d")).unwrap();
        std::fs::write(mnt.join("d/x"), b"y").unwrap();
        assert!(mnt.join("d/x").exists());
        // (5) rename
        std::fs::rename(mnt.join("new.txt"), mnt.join("renamed.txt")).unwrap();
        assert!(!mnt.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(mnt.join("renamed.txt")).unwrap(),
            "created\nmore\n"
        );
        // (6) delete a baseline file → hidden
        std::fs::remove_file(mnt.join("src/main.rs")).unwrap();
        assert!(!mnt.join("src/main.rs").exists());

        // the synthetic .git is protected from deletion/replacement (§6)
        assert!(std::fs::remove_file(mnt.join(".git")).is_err());

        mount.unmount();
    }
}
