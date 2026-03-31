//! LD_PRELOAD shim — intercepts file ops for /reevofs/* paths.
//! All other paths pass through with zero overhead.
//!
//! Strategy: When a /reevofs/* path is opened, fetch content via HTTP,
//! write it to a memfd (or pipe), and return the real kernel FD.
//! This means fstat/read/close/lseek all work natively on the kernel FD.

#![allow(unsafe_op_in_unsafe_fn)]
#![allow(unused_variables)]

use std::cell::Cell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

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

fn set_errno(err: c_int) {
    unsafe {
        #[cfg(target_os = "linux")]
        { *libc::__errno_location() = err; }
        #[cfg(target_os = "macos")]
        { *libc::__error() = err; }
    }
}

// ---------------------------------------------------------------------------
// Create a real kernel FD with content using memfd_create
// Falls back to pipe if memfd is unavailable.
// ---------------------------------------------------------------------------

fn create_fd_with_content(content: &[u8]) -> c_int {
    unsafe {
        // Try memfd_create first (supports fstat, lseek, mmap)
        let name = b"reevofs\0";
        #[cfg(target_os = "linux")]
            let fd = {
                #[cfg(target_arch = "x86_64")]
                const SYS_MEMFD: libc::c_long = 319;
                #[cfg(target_arch = "aarch64")]
                const SYS_MEMFD: libc::c_long = 279;
                libc::syscall(SYS_MEMFD, name.as_ptr(), 0 as c_int) as c_int
            };
            #[cfg(not(target_os = "linux"))]
            let fd: c_int = -1;
        if fd >= 0 {
            let mut written = 0usize;
            while written < content.len() {
                let n = libc::write(fd, content[written..].as_ptr() as *const c_void, content.len() - written);
                if n < 0 { break; }
                written += n as usize;
            }
            libc::lseek(fd, 0, libc::SEEK_SET);
            return fd;
        }

        // Fallback: pipe (doesn't support lseek/fstat size, but read works)
        let mut fds = [0 as c_int; 2];
        if libc::pipe(fds.as_mut_ptr()) == 0 {
            let mut written = 0usize;
            while written < content.len() {
                let n = libc::write(fds[1], content[written..].as_ptr() as *const c_void, content.len() - written);
                if n < 0 { break; }
                written += n as usize;
            }
            libc::close(fds[1]);
            return fds[0]; // read end
        }

        -1
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
        Ok(resp) => {
            let fd = create_fd_with_content(resp.content.as_bytes());
            if fd < 0 {
                set_errno(libc::EIO);
            }
            Some(fd)
        }
        Err(_) => {
            set_errno(libc::ENOENT);
            Some(-1)
        }
    }
}

// ---------------------------------------------------------------------------
// openat — what modern glibc/Python actually uses
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
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
// open / open64
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
// stat / lstat / fstatat — intercept path-based stat for /reevofs/*
// (fstat works natively since we return real kernel FDs now)
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
// access / faccessat
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
// 64-bit variants — CPython on aarch64 glibc 2.33+ uses fstatat64, stat64, etc.
// Each has its own dlsym fallback to avoid circular calls.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatat64(dirfd: c_int, path: *const c_char, buf: *mut libc::stat, flag: c_int) -> c_int {
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
    let real: F = std::mem::transmute(dlsym_next(b"fstatat64\0"));
    real(dirfd, path, buf, flag)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat64(path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_stat_reevofs(s, buf) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"stat64\0"));
    real(path, buf)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat64(path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_stat_reevofs(s, buf) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"lstat64\0"));
    real(path, buf)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat64(dirfd: c_int, path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if s.starts_with('/') {
                if let Some(result) = try_open_reevofs(s) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, libc::mode_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"openat64\0"));
    real(dirfd, path, flags, mode)
}
