//! LD_PRELOAD shim — intercepts file ops for /reevofs/* paths.
//! All other paths pass through with zero overhead.
//!
//! Intercepts: openat, open, open64, read, close, fstatat, stat, lstat, access
//! Modern glibc uses openat/fstatat instead of open/stat, so both are needed.

#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use reevofs_api::ReevoClient;

// ---------------------------------------------------------------------------
// Re-entrancy guard
// ---------------------------------------------------------------------------

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

struct ReentrancyGuard;

impl ReentrancyGuard {
    fn try_enter() -> Option<Self> {
        IN_HOOK.with(|flag| {
            if flag.get() {
                None
            } else {
                flag.set(true);
                Some(ReentrancyGuard)
            }
        })
    }
}

impl Drop for ReentrancyGuard {
    fn drop(&mut self) {
        IN_HOOK.with(|flag| flag.set(false));
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const DEFAULT_PREFIX: &str = "/reevofs";

struct Config {
    prefix: String,
    namespace: String,
    scope: String,
    client: ReevoClient,
}

static CONFIG: Lazy<Option<Config>> = Lazy::new(|| {
    let _guard = ReentrancyGuard::try_enter();

    let api_url = std::env::var("REEVO_API_URL").ok()?;
    let token = std::env::var("REEVO_API_TOKEN").unwrap_or_default();
    let user_id = std::env::var("REEVO_USER_ID").ok();
    let org_id = std::env::var("REEVO_ORG_ID").ok();
    let prefix = std::env::var("REEVOFS_MOUNT_PREFIX").unwrap_or_else(|_| DEFAULT_PREFIX.into());
    let namespace = std::env::var("REEVOFS_NAMESPACE").unwrap_or_else(|_| "skills".into());
    let scope = std::env::var("REEVOFS_SCOPE").unwrap_or_else(|_| "org".into());

    Some(Config {
        prefix,
        namespace,
        scope,
        client: ReevoClient::with_ids(&api_url, &token, user_id.as_deref(), org_id.as_deref()),
    })
});

fn quick_prefix_match(path_str: &str) -> bool {
    path_str.starts_with(DEFAULT_PREFIX)
}

fn match_path(path_str: &str) -> Option<(&'static Config, &str)> {
    let cfg = CONFIG.as_ref()?;
    if path_str.starts_with(&cfg.prefix) {
        let rest = &path_str[cfg.prefix.len()..];
        Some((cfg, if rest.is_empty() { "/" } else { rest }))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// dlsym
// ---------------------------------------------------------------------------

unsafe fn dlsym_next(name: &[u8]) -> *mut c_void {
    libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const c_char)
}

// ---------------------------------------------------------------------------
// Virtual FD table (900_000+ range)
// ---------------------------------------------------------------------------

struct VirtualFile {
    content: Vec<u8>,
    offset: usize,
}

static VIRTUAL_FDS: Lazy<Mutex<HashMap<c_int, VirtualFile>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_VFD: Lazy<Mutex<c_int>> = Lazy::new(|| Mutex::new(900_000));

fn alloc_vfd(content: Vec<u8>) -> c_int {
    let mut next = NEXT_VFD.lock().unwrap();
    let fd = *next;
    *next += 1;
    VIRTUAL_FDS.lock().unwrap().insert(fd, VirtualFile { content, offset: 0 });
    fd
}

fn is_virtual_fd(fd: c_int) -> bool { fd >= 900_000 }

fn set_errno(err: c_int) {
    unsafe {
        #[cfg(target_os = "linux")]
        { *libc::__errno_location() = err; }
        #[cfg(target_os = "macos")]
        { *libc::__error() = err; }
    }
}

// ---------------------------------------------------------------------------
// Core open logic — shared by open, open64, openat
// ---------------------------------------------------------------------------

fn try_open_reevofs(path_str: &str) -> Option<c_int> {
    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, api_path) = match_path(path_str)?;
    match cfg.client.read_file(&cfg.namespace, &cfg.scope, api_path) {
        Ok(resp) => Some(alloc_vfd(resp.content.into_bytes())),
        Err(_) => {
            set_errno(libc::ENOENT);
            Some(-1)
        }
    }
}

// ---------------------------------------------------------------------------
// openat — this is what modern glibc/Python actually uses
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            // Only intercept absolute paths (dirfd is ignored for absolute paths)
            if s.starts_with('/') {
                if let Some(result) = try_open_reevofs(s) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, libc::mode_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"openat\0"));
    real(dirfd, path, flags, mode)
}

// ---------------------------------------------------------------------------
// open / open64 — legacy, but still used by some programs
// ---------------------------------------------------------------------------

unsafe fn do_open(path: *const c_char, flags: c_int, mode: libc::mode_t, sym: &[u8]) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_open_reevofs(s) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, c_int, libc::mode_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(sym));
    real(path, flags, mode)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    do_open(path, flags, mode, b"open\0")
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    do_open(path, flags, mode, b"open64\0")
}

// ---------------------------------------------------------------------------
// read / close — only intercept virtual FDs
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, count: libc::size_t) -> libc::ssize_t {
    if is_virtual_fd(fd) {
        let mut fds = VIRTUAL_FDS.lock().unwrap();
        if let Some(vf) = fds.get_mut(&fd) {
            let remaining = vf.content.len().saturating_sub(vf.offset);
            let n = std::cmp::min(count, remaining);
            if n > 0 {
                std::ptr::copy_nonoverlapping(vf.content[vf.offset..].as_ptr(), buf as *mut u8, n);
                vf.offset += n;
            }
            return n as libc::ssize_t;
        }
        set_errno(libc::EBADF);
        return -1;
    }
    type F = unsafe extern "C" fn(c_int, *mut c_void, libc::size_t) -> libc::ssize_t;
    let real: F = std::mem::transmute(dlsym_next(b"read\0"));
    real(fd, buf, count)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    if is_virtual_fd(fd) {
        VIRTUAL_FDS.lock().unwrap().remove(&fd);
        return 0;
    }
    type F = unsafe extern "C" fn(c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"close\0"));
    real(fd)
}

// ---------------------------------------------------------------------------
// fstatat (aka newfstatat) — what modern glibc/Python uses for stat()
// Also intercept __xstat for older glibc
// ---------------------------------------------------------------------------

fn try_stat_reevofs(path_str: &str, buf: *mut libc::stat) -> Option<c_int> {
    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, api_path) = match_path(path_str)?;

    unsafe { std::ptr::write_bytes(buf, 0, 1); }

    if let Ok(resp) = cfg.client.read_file(&cfg.namespace, &cfg.scope, api_path) {
        unsafe {
            (*buf).st_mode = libc::S_IFREG | 0o644;
            (*buf).st_size = resp.content.len() as libc::off_t;
            (*buf).st_nlink = 1;
        }
        return Some(0);
    }
    if cfg.client.list_dir(&cfg.namespace, &cfg.scope, api_path).is_ok() {
        unsafe {
            (*buf).st_mode = libc::S_IFDIR | 0o755;
            (*buf).st_nlink = 2;
        }
        return Some(0);
    }
    set_errno(libc::ENOENT);
    Some(-1)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatat(dirfd: c_int, path: *const c_char, buf: *mut libc::stat, flag: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if s.starts_with('/') {
                if let Some(result) = try_stat_reevofs(s, buf) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, *mut libc::stat, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"fstatat\0"));
    real(dirfd, path, buf, flag)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __fxstatat(ver: c_int, dirfd: c_int, path: *const c_char, buf: *mut libc::stat, flag: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if s.starts_with('/') {
                if let Some(result) = try_stat_reevofs(s, buf) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, c_int, *const c_char, *mut libc::stat, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"__fxstatat\0"));
    real(ver, dirfd, path, buf, flag)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_stat_reevofs(s, buf) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"stat\0"));
    real(path, buf)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_stat_reevofs(s, buf) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"lstat\0"));
    real(path, buf)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __xstat(ver: c_int, path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_stat_reevofs(s, buf) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"__xstat\0"));
    real(ver, path, buf)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __lxstat(ver: c_int, path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_stat_reevofs(s, buf) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"__lxstat\0"));
    real(ver, path, buf)
}

// ---------------------------------------------------------------------------
// access
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if quick_prefix_match(s) {
                return 0;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"access\0"));
    real(path, mode)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn faccessat(dirfd: c_int, path: *const c_char, mode: c_int, flags: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if s.starts_with('/') && quick_prefix_match(s) {
                return 0;
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"faccessat\0"));
    real(dirfd, path, mode, flags)
}

// ---------------------------------------------------------------------------
// fstat — Python calls fstat(fd) right after openat() returns our VFD
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstat(fd: c_int, buf: *mut libc::stat) -> c_int {
    if is_virtual_fd(fd) {
        let fds = VIRTUAL_FDS.lock().unwrap();
        if let Some(vf) = fds.get(&fd) {
            std::ptr::write_bytes(buf, 0, 1);
            (*buf).st_mode = libc::S_IFREG | 0o644;
            (*buf).st_size = vf.content.len() as libc::off_t;
            (*buf).st_nlink = 1;
            return 0;
        }
        set_errno(libc::EBADF);
        return -1;
    }
    type F = unsafe extern "C" fn(c_int, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"fstat\0"));
    real(fd, buf)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __fxstat(ver: c_int, fd: c_int, buf: *mut libc::stat) -> c_int {
    if is_virtual_fd(fd) {
        return fstat(fd, buf);
    }
    type F = unsafe extern "C" fn(c_int, c_int, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"__fxstat\0"));
    real(ver, fd, buf)
}
