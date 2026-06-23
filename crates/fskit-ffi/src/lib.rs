//! C ABI over the FSKit `FSVolume` bridge (issue #5, spec §41).
//!
//! The on-device Swift FSKit extension (`crates/fs-fskit/extension/`) links this
//! static library and calls these `extern "C"` functions from its
//! `FSUnaryFileSystem` / `FSVolume` operations. Everything below is a thin,
//! panic-guarded marshalling layer over [`glm_fs_fskit::FskitOps`]; all real
//! logic lives in the shared engine.
//!
//! Conventions:
//! * Operations return an **errno** (`0` = success; otherwise a positive POSIX
//!   errno suitable for `fs_errorForPOSIXError`). On error, a human-readable
//!   message is stashed for [`glm_fskit_last_error`].
//! * Names and link targets are passed as `(ptr, len)` byte buffers — the exact
//!   bytes Git recorded (spec §41). They are never assumed UTF-8.
//! * The handle is created by [`glm_fskit_open`] and freed by
//!   [`glm_fskit_close`]. It is `Send + Sync`; the Swift side may call from
//!   multiple threads (FskitOps is internally `Mutex`-guarded).

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::slice;

use glm_core::{Error, ErrorCode};
use glm_daemon::{Controller, OpenMount};
use glm_fs_fskit::{AppleVolume, FskitOps};
use glm_platform::DataRoots;
use glm_workspace::EntryKind;
use serde::Deserialize;

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_last_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.into());
}

/// errno for an internal/panic failure (`EIO`).
const EIO: i32 = 5;
/// errno for a bad argument from the caller (`EINVAL`).
const EINVAL: i32 = 22;

/// A C-ABI snapshot of a file's attributes (neutral; spec §28).
#[repr(C)]
pub struct GlmAttr {
    /// Inode number.
    pub ino: u64,
    /// Inode generation.
    pub generation: u64,
    /// Exact size in bytes (0 for directories).
    pub size: u64,
    /// 0 = regular file, 1 = directory, 2 = symlink, 3 = gitlink/submodule.
    pub kind: u32,
    /// POSIX `st_mode` (type + permission bits).
    pub mode: u32,
}

fn kind_code(kind: EntryKind) -> u32 {
    match kind {
        EntryKind::File { .. } => 0,
        EntryKind::Dir => 1,
        EntryKind::Symlink => 2,
        EntryKind::Gitlink => 3,
    }
}

fn to_glm_attr(a: &glm_fs_common::FileAttr) -> GlmAttr {
    GlmAttr {
        ino: a.ino,
        generation: a.generation,
        size: a.size,
        kind: kind_code(a.kind),
        mode: a.unix_mode,
    }
}

/// The opaque handle the Swift side holds for the lifetime of a mounted volume.
pub struct GlmHandle {
    ops: FskitOps,
}

#[derive(Deserialize)]
struct OpenConfig {
    /// Ephemeral data-root base (tests / `GLM_DATA_ROOT`); falls back to the
    /// per-user roots when absent.
    data_root: Option<String>,
    /// The registered mountpoint to open.
    mountpoint: String,
    /// `"case_sensitive"` for a case-sensitive APFS volume; default is
    /// case-insensitive.
    #[serde(default)]
    volume: String,
}

/// Translate an engine error into a POSIX errno and record its message.
fn errno_of(e: &Error) -> i32 {
    set_last_error(format!("{e}"));
    e.errno()
}

/// Run `f`, converting a panic into `EIO` with a recorded message.
fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => {
            set_last_error("internal panic in glm-fskit-ffi");
            EIO
        }
    }
}

unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        slice::from_raw_parts(ptr, len)
    }
}

unsafe fn handle<'a>(h: *mut GlmHandle) -> Option<&'a GlmHandle> {
    if h.is_null() {
        None
    } else {
        Some(&*h)
    }
}

/// Open the workspace registered at a mountpoint and return a handle, or null on
/// failure (see [`glm_fskit_last_error`]). `config_json` is a UTF-8 JSON object
/// (`{"mountpoint": "...", "data_root": "...", "volume": "case_sensitive"}`).
///
/// # Safety
/// `config_ptr`/`config_len` must describe a valid byte range for the call.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_open(
    config_ptr: *const u8,
    config_len: usize,
) -> *mut GlmHandle {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let raw = bytes(config_ptr, config_len);
        let cfg: OpenConfig = serde_json::from_slice(raw)
            .map_err(|e| Error::new(ErrorCode::Configuration, format!("bad config json: {e}")))?;
        let ctl = match cfg.data_root {
            Some(root) => Controller::new(DataRoots::ephemeral(PathBuf::from(root))),
            None => Controller::for_user(),
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let spec = ctl.resolve_mount(Some(std::path::Path::new(&cfg.mountpoint)), &cwd)?;
        let OpenMount { workspace, .. } = ctl.open(&spec, None)?;
        let volume = if cfg.volume == "case_sensitive" {
            AppleVolume::CaseSensitive
        } else {
            AppleVolume::CaseInsensitive
        };
        Ok::<_, Error>(GlmHandle {
            ops: FskitOps::with_volume(workspace, volume),
        })
    }));
    match result {
        Ok(Ok(h)) => Box::into_raw(Box::new(h)),
        Ok(Err(e)) => {
            set_last_error(format!("{e}"));
            std::ptr::null_mut()
        }
        Err(_) => {
            set_last_error("internal panic opening workspace");
            std::ptr::null_mut()
        }
    }
}

/// Free a handle from [`glm_fskit_open`].
///
/// # Safety
/// `h` must be a handle returned by [`glm_fskit_open`] and not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_close(h: *mut GlmHandle) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}

/// Copy the last error message (UTF-8) into `buf`; returns the message's full
/// byte length (which may exceed `cap`).
///
/// # Safety
/// `buf` must be valid for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_last_error(buf: *mut u8, cap: usize) -> usize {
    LAST_ERROR.with(|e| {
        let msg = e.borrow();
        let src = msg.as_bytes();
        if !buf.is_null() && cap > 0 {
            let n = src.len().min(cap);
            std::ptr::copy_nonoverlapping(src.as_ptr(), buf, n);
        }
        src.len()
    })
}

/// FSKit `lookupName`: resolve `name` in `parent_ino`, filling `out`.
///
/// # Safety
/// Pointers must be valid for the call; `out` must point at a writable `GlmAttr`.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_lookup(
    h: *mut GlmHandle,
    parent_ino: u64,
    name_ptr: *const u8,
    name_len: usize,
    out: *mut GlmAttr,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.lookup(parent_ino, bytes(name_ptr, name_len)) {
            Ok(a) => {
                if !out.is_null() {
                    *out = to_glm_attr(&a);
                }
                0
            }
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `getAttributes`.
///
/// # Safety
/// `out` must point at a writable `GlmAttr`.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_getattr(h: *mut GlmHandle, ino: u64, out: *mut GlmAttr) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.getattr(ino) {
            Ok(a) => {
                if !out.is_null() {
                    *out = to_glm_attr(&a);
                }
                0
            }
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `read`: copy up to `cap` bytes from `offset` into `buf`; `*out_len`
/// receives the number of bytes copied.
///
/// # Safety
/// `buf` must be valid for `cap` bytes; `out_len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_read(
    h: *mut GlmHandle,
    ino: u64,
    offset: u64,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.read(ino, offset, cap as u32) {
            Ok(data) => {
                let n = data.len().min(cap);
                if !buf.is_null() && n > 0 {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), buf, n);
                }
                if !out_len.is_null() {
                    *out_len = n;
                }
                0
            }
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `readSymbolicLink`: copy the link target bytes into `buf`; `*out_len`
/// receives the full target length (which may exceed `cap`).
///
/// # Safety
/// `buf` must be valid for `cap` bytes; `out_len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_readlink(
    h: *mut GlmHandle,
    ino: u64,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.readlink(ino) {
            Ok(data) => {
                let n = data.len().min(cap);
                if !buf.is_null() && n > 0 {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), buf, n);
                }
                if !out_len.is_null() {
                    *out_len = data.len();
                }
                0
            }
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `reclaim`: drop kernel references for an inode.
///
/// # Safety
/// `h` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_forget(h: *mut GlmHandle, ino: u64, n: u64) {
    let _ = guard(|| {
        if let Some(h) = handle(h) {
            h.ops.forget(ino, n);
        }
        0
    });
}

/// The signature of the per-entry callback used by [`glm_fskit_enumerate`].
/// Returning `false` stops enumeration (e.g. the reply buffer is full).
pub type EnumerateCallback = extern "C" fn(
    ctx: *mut c_void,
    name_ptr: *const u8,
    name_len: usize,
    attr: *const GlmAttr,
) -> bool;

/// FSKit `enumerateDirectory`: invoke `cb` once per child of `ino` with the
/// entry's exact recorded name and attributes (spec §41).
///
/// # Safety
/// `cb` must be a valid function pointer; `ctx` is passed back to it verbatim.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_enumerate(
    h: *mut GlmHandle,
    ino: u64,
    ctx: *mut c_void,
    cb: EnumerateCallback,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.enumerate(ino) {
            Ok(entries) => {
                for e in entries {
                    let attr = to_glm_attr(&e.attr);
                    let cont = cb(ctx, e.name.as_ptr(), e.name.len(), &attr);
                    if !cont {
                        break;
                    }
                }
                0
            }
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `createItem` (regular file): create/replace an empty file, filling `out`.
///
/// # Safety
/// Pointers must be valid; `out` must point at a writable `GlmAttr`.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_create(
    h: *mut GlmHandle,
    parent_ino: u64,
    name_ptr: *const u8,
    name_len: usize,
    executable: bool,
    out: *mut GlmAttr,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h
            .ops
            .create(parent_ino, bytes(name_ptr, name_len), executable)
        {
            Ok(a) => {
                if !out.is_null() {
                    *out = to_glm_attr(&a);
                }
                0
            }
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `createItem` (symlink).
///
/// # Safety
/// Pointers must be valid; `out` must point at a writable `GlmAttr`.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_symlink(
    h: *mut GlmHandle,
    parent_ino: u64,
    name_ptr: *const u8,
    name_len: usize,
    target_ptr: *const u8,
    target_len: usize,
    out: *mut GlmAttr,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.symlink(
            parent_ino,
            bytes(name_ptr, name_len),
            bytes(target_ptr, target_len),
        ) {
            Ok(a) => {
                if !out.is_null() {
                    *out = to_glm_attr(&a);
                }
                0
            }
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `write`: write `data` at `offset`; `*out_written` receives the count.
///
/// # Safety
/// `data`/`out_written` must be valid for the call.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_write(
    h: *mut GlmHandle,
    ino: u64,
    offset: u64,
    data_ptr: *const u8,
    data_len: usize,
    out_written: *mut u32,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.write(ino, offset, bytes(data_ptr, data_len)) {
            Ok(n) => {
                if !out_written.is_null() {
                    *out_written = n;
                }
                0
            }
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `setAttributes` (size): truncate/extend `ino` to `len`.
///
/// # Safety
/// `h` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_truncate(h: *mut GlmHandle, ino: u64, len: u64) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.truncate(ino, len) {
            Ok(()) => 0,
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `setAttributes` (mode): set/clear the executable bit.
///
/// # Safety
/// `h` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_set_executable(
    h: *mut GlmHandle,
    ino: u64,
    executable: bool,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.set_executable(ino, executable) {
            Ok(()) => 0,
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `removeItem`.
///
/// # Safety
/// Pointers must be valid for the call.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_remove(
    h: *mut GlmHandle,
    parent_ino: u64,
    name_ptr: *const u8,
    name_len: usize,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.remove(parent_ino, bytes(name_ptr, name_len)) {
            Ok(()) => 0,
            Err(e) => errno_of(&e),
        }
    })
}

/// FSKit `renameItem`.
///
/// # Safety
/// Pointers must be valid for the call.
#[no_mangle]
pub unsafe extern "C" fn glm_fskit_rename(
    h: *mut GlmHandle,
    parent_ino: u64,
    name_ptr: *const u8,
    name_len: usize,
    new_parent_ino: u64,
    new_name_ptr: *const u8,
    new_name_len: usize,
) -> i32 {
    guard(|| {
        let Some(h) = handle(h) else { return EINVAL };
        match h.ops.rename(
            parent_ino,
            bytes(name_ptr, name_len),
            new_parent_ino,
            bytes(new_name_ptr, new_name_len),
        ) {
            Ok(()) => 0,
            Err(e) => errno_of(&e),
        }
    })
}

/// The root inode number (FSKit/`FuseOps` convention).
#[no_mangle]
pub extern "C" fn glm_fskit_root_ino() -> u64 {
    glm_fs_common::ROOT_INO
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use glm_core::RepoPath;
    use glm_git_store::{FetchOptions, GitStore};
    use glm_object_provider::{GitObjectProvider, ObjectProvider};
    use glm_workspace::{Workspace, WorkspaceConfig};

    // Build a handle directly over a seeded workspace (bypassing the daemon
    // registry), to exercise the C ABI marshalling end-to-end.
    fn handle_with(
        files: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, glm_testkit::SeededRemote, GlmHandle) {
        let remote = glm_testkit::seed_remote(files);
        let tmp = tempfile::tempdir().unwrap();
        let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
        store.set_config("protocol.file.allow", "always").unwrap();
        store.set_config("core.autocrlf", "false").unwrap();
        store.add_remote("origin", &remote.url).unwrap();
        store
            .fetch(
                "origin",
                &[],
                &FetchOptions {
                    filter: Some("blob:none".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let base = store
            .resolve_ref("refs/remotes/origin/main")
            .unwrap()
            .unwrap();
        let provider: Arc<dyn ObjectProvider> =
            Arc::new(GitObjectProvider::with_git_fetcher(store.clone()));
        let cfg = WorkspaceConfig {
            workspace_head_ref: "refs/lazy-mount/workspaces/ffi/head".into(),
            attached_branch: None,
            remote: Some("origin".into()),
            identity: None,
        };
        let ws = Workspace::open_or_create(store, provider, tmp.path(), cfg, Some(base)).unwrap();
        let h = GlmHandle {
            ops: FskitOps::new(ws),
        };
        (tmp, remote, h)
    }

    #[test]
    fn lookup_read_and_enumerate_over_ffi() {
        let (_tmp, _remote, h) = handle_with(&[("a.txt", b"hello\n"), ("src/lib.rs", b"x\n")]);
        let hp: *mut GlmHandle = &h as *const GlmHandle as *mut GlmHandle;
        let root = glm_fskit_root_ino();

        // lookup a.txt
        let mut attr = GlmAttr {
            ino: 0,
            generation: 0,
            size: 0,
            kind: 9,
            mode: 0,
        };
        let rc = unsafe { glm_fskit_lookup(hp, root, b"a.txt".as_ptr(), 5, &mut attr) };
        assert_eq!(rc, 0);
        assert_eq!(attr.size, 6);
        assert_eq!(attr.kind, 0); // file

        // read it
        let mut buf = [0u8; 64];
        let mut n = 0usize;
        let rc = unsafe { glm_fskit_read(hp, attr.ino, 0, buf.as_mut_ptr(), buf.len(), &mut n) };
        assert_eq!(rc, 0);
        assert_eq!(&buf[..n], b"hello\n");

        // enumerate root via the callback
        extern "C" fn collect(
            ctx: *mut c_void,
            name_ptr: *const u8,
            name_len: usize,
            _attr: *const GlmAttr,
        ) -> bool {
            let names = unsafe { &mut *(ctx as *mut Vec<Vec<u8>>) };
            let name = unsafe { slice::from_raw_parts(name_ptr, name_len) };
            names.push(name.to_vec());
            true
        }
        let mut names: Vec<Vec<u8>> = Vec::new();
        let rc = unsafe {
            glm_fskit_enumerate(
                hp,
                root,
                &mut names as *mut Vec<Vec<u8>> as *mut c_void,
                collect,
            )
        };
        assert_eq!(rc, 0);
        assert!(names.contains(&b"a.txt".to_vec()));
        assert!(names.contains(&b"src".to_vec()));
    }

    #[test]
    fn write_routes_through_overlay_over_ffi() {
        let (_tmp, _remote, h) = handle_with(&[("a.txt", b"hi\n")]);
        let hp: *mut GlmHandle = &h as *const GlmHandle as *mut GlmHandle;
        let root = glm_fskit_root_ino();

        let mut attr = GlmAttr {
            ino: 0,
            generation: 0,
            size: 0,
            kind: 9,
            mode: 0,
        };
        let rc = unsafe { glm_fskit_create(hp, root, b"new.txt".as_ptr(), 7, false, &mut attr) };
        assert_eq!(rc, 0);
        let mut written = 0u32;
        let rc = unsafe { glm_fskit_write(hp, attr.ino, 0, b"world\n".as_ptr(), 6, &mut written) };
        assert_eq!(rc, 0);
        assert_eq!(written, 6);

        let p = RepoPath::from_bytes(b"new.txt".to_vec()).unwrap();
        assert_eq!(
            h.ops
                .workspace()
                .read_file(&p, glm_core::FetchPolicy::AllowNetwork)
                .unwrap(),
            b"world\n"
        );
    }
}
