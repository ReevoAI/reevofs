//! LD_PRELOAD shim that intercepts libc filesystem calls and redirects
//! paths under `/reevofs/` to the Reevo API via reevofs-api.
//!
//! Usage:
//!   LD_PRELOAD=/usr/local/lib/libreevofs_preload.so some_command
//!
//! Environment variables (same as reevofs CLI):
//!   REEVO_API_URL, REEVO_API_TOKEN, REEVO_USER_ID, REEVO_ORG_ID
//!   REEVOFS_MOUNT_PREFIX (default: /reevofs)
//!   REEVOFS_NAMESPACE (default: skills)
//!   REEVOFS_SCOPE (default: org)

#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use reevofs_api::ReevoClient;

// ---------------------------------------------------------------------------
// Config (read once from env)
// ---------------------------------------------------------------------------

struct Config {
    prefix: String,
    namespace: String,
    scope: String,
    client: ReevoClient,
}

static CONFIG: Lazy<Option<Config>> = Lazy::new(|| {
    let api_url = std::env::var("REEVO_API_URL").ok()?;
    let token = std::env::var("REEVO_API_TOKEN").unwrap_or_default();
    let user_id = std::env::var("REEVO_USER_ID").ok();
    let org_id = std::env::var("REEVO_ORG_ID").ok();
    let prefix = std::env::var("REEVOFS_MOUNT_PREFIX").unwrap_or_else(|_| "/reevofs".into());
    let namespace = std::env::var("REEVOFS_NAMESPACE").unwrap_or_else(|_| "skills".into());
    let scope = std::env::var("REEVOFS_SCOPE").unwrap_or_else(|_| "org".into());

    let client = ReevoClient::with_ids(
        &api_url,
        &token,
        user_id.as_deref(),
        org_id.as_deref(),
    );

    Some(Config { prefix, namespace, scope, client })
});

fn config() -> Option<&'static Config> {
    CONFIG.as_ref()
}

/// If `path` starts with our prefix, return the remainder. Otherwise None.
fn strip_prefix(path: &str) -> Option<&str> {
    let cfg = config()?;
    if path.starts_with(&cfg.prefix) {
        let rest = &path[cfg.prefix.len()..];
        Some(if rest.is_empty() { "/" } else { rest })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// dlsym helper
// ---------------------------------------------------------------------------

unsafe fn dlsym_next(name: &[u8]) -> *mut c_void {
    libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const c_char)
}

// ---------------------------------------------------------------------------
// Virtual file descriptor table
// ---------------------------------------------------------------------------

struct VirtualFile {
    content: Vec<u8>,
    offset: usize,
}

static VIRTUAL_FDS: Lazy<Mutex<HashMap<c_int, VirtualFile>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static NEXT_VFD: Lazy<Mutex<c_int>> = Lazy::new(|| Mutex::new(-1000));

fn alloc_vfd(content: Vec<u8>) -> c_int {
    let mut next = NEXT_VFD.lock().unwrap();
    let fd = *next;
    *next -= 1;
    VIRTUAL_FDS.lock().unwrap().insert(fd, VirtualFile { content, offset: 0 });
    fd
}

fn is_virtual_fd(fd: c_int) -> bool {
    fd <= -1000
}

fn set_errno(err: c_int) {
    unsafe {
        #[cfg(target_os = "linux")]
        { *libc::__errno_location() = err; }
        #[cfg(target_os = "macos")]
        { *libc::__error() = err; }
    }
}

// ---------------------------------------------------------------------------
// Intercepted libc functions
// ---------------------------------------------------------------------------

/// open(path, flags, ...) -> fd
///
/// We use open64 as the intercept point since glibc routes most opens through it.
/// We don't use C variadics (unstable in Rust) — instead we accept a fixed mode_t
/// param which is safe because the ABI puts it in the same register regardless.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    type OpenFn = unsafe extern "C" fn(*const c_char, c_int, libc::mode_t) -> c_int;
    let real: OpenFn = std::mem::transmute(dlsym_next(b"open64\0"));

    if path.is_null() {
        return real(path, flags, mode);
    }

    if let Ok(path_str) = CStr::from_ptr(path).to_str() {
        if let Some(api_path) = strip_prefix(path_str) {
            if let Some(cfg) = config() {
                if let Ok(resp) = cfg.client.read_file(&cfg.namespace, &cfg.scope, api_path) {
                    return alloc_vfd(resp.content.into_bytes());
                }
            }
            set_errno(libc::ENOENT);
            return -1;
        }
    }

    real(path, flags, mode)
}

/// Also intercept plain open() which some programs call directly
#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    open64(path, flags, mode)
}

/// read(fd, buf, count) -> ssize_t
#[unsafe(no_mangle)]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, count: libc::size_t) -> libc::ssize_t {
    if is_virtual_fd(fd) {
        let mut fds = VIRTUAL_FDS.lock().unwrap();
        if let Some(vf) = fds.get_mut(&fd) {
            let remaining = vf.content.len().saturating_sub(vf.offset);
            let to_read = std::cmp::min(count, remaining);
            if to_read > 0 {
                std::ptr::copy_nonoverlapping(
                    vf.content[vf.offset..].as_ptr(),
                    buf as *mut u8,
                    to_read,
                );
                vf.offset += to_read;
            }
            return to_read as libc::ssize_t;
        }
        set_errno(libc::EBADF);
        return -1;
    }

    type ReadFn = unsafe extern "C" fn(c_int, *mut c_void, libc::size_t) -> libc::ssize_t;
    let real: ReadFn = std::mem::transmute(dlsym_next(b"read\0"));
    real(fd, buf, count)
}

/// close(fd) -> int
#[unsafe(no_mangle)]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    if is_virtual_fd(fd) {
        VIRTUAL_FDS.lock().unwrap().remove(&fd);
        return 0;
    }

    type CloseFn = unsafe extern "C" fn(c_int) -> c_int;
    let real: CloseFn = std::mem::transmute(dlsym_next(b"close\0"));
    real(fd)
}

/// stat/lstat — make /reevofs paths appear as files or directories.
/// On aarch64 glibc there's no __xstat; stat is called directly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    type StatFn = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
    let real: StatFn = std::mem::transmute(dlsym_next(b"stat\0"));

    if !path.is_null() {
        if let Ok(path_str) = CStr::from_ptr(path).to_str() {
            if let Some(api_path) = strip_prefix(path_str) {
                std::ptr::write_bytes(buf, 0, 1);
                if let Some(cfg) = config() {
                    if let Ok(resp) = cfg.client.read_file(&cfg.namespace, &cfg.scope, api_path) {
                        (*buf).st_mode = libc::S_IFREG | 0o644;
                        (*buf).st_size = resp.content.len() as libc::off_t;
                        (*buf).st_nlink = 1;
                        return 0;
                    }
                    if cfg.client.list_dir(&cfg.namespace, &cfg.scope, api_path).is_ok() {
                        (*buf).st_mode = libc::S_IFDIR | 0o755;
                        (*buf).st_nlink = 2;
                        return 0;
                    }
                }
                set_errno(libc::ENOENT);
                return -1;
            }
        }
    }

    real(path, buf)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    // No symlinks in reevofs — delegate to stat
    stat(path, buf)
}

/// access() — report /reevofs paths as accessible
#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(path_str) = CStr::from_ptr(path).to_str() {
            if strip_prefix(path_str).is_some() {
                return 0;
            }
        }
    }

    type AccessFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
    let real: AccessFn = std::mem::transmute(dlsym_next(b"access\0"));
    real(path, mode)
}
