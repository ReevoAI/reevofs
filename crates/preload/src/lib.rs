//! # reevofs LD_PRELOAD shim
//!
//! Intercepts libc file operations for paths under `/reevofs/` and transparently
//! serves them via HTTP from the Reevo API. All other paths pass through to the
//! real libc with zero overhead.
//!
//! ## Architecture
//!
//! ```text
//!   Process calls open("/reevofs/skills/foo.md")
//!       │
//!       ▼
//!   LD_PRELOAD shim (this library)
//!       │
//!       ├─ Path starts with /reevofs? ── No ──► dlsym(RTLD_NEXT) → real libc
//!       │
//!       └─ Yes: fetch via HTTP ──► reevofs-api crate ──► Reevo backend
//!              │
//!              ▼
//!          Write content to memfd_create() (anonymous kernel FD)
//!              │
//!              ▼
//!          Return real FD to caller
//!          (fstat/read/lseek/close all work natively on it)
//! ```
//!
//! ## Why LD_PRELOAD instead of FUSE?
//!
//! - FUSE requires `fusermount` / `/dev/fuse` access which most container runtimes
//!   (including AWS AgentCore) don't provide.
//! - LD_PRELOAD works in any unprivileged container — no kernel module needed.
//! - The shim is stateless: each open() does a fresh HTTP fetch, which is fine for
//!   the read-only, small-file use case of loading skill definitions.
//!
//! ## Hooked functions
//!
//! We hook every variant of open/stat that programs might use, covering:
//! - POSIX: `open`, `openat`, `stat`, `lstat`, `fstatat`, `access`, `faccessat`
//! - glibc legacy wrappers: `__xstat`, `__lxstat`, `__fxstatat`
//! - 64-bit variants (glibc 2.33+ on aarch64): `open64`, `openat64`, `stat64`,
//!   `lstat64`, `fstatat64`
//!
//! We do NOT hook `fstat`/`read`/`close`/`lseek` — those operate on the real
//! memfd kernel FD and work natively.

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
// The HTTP client (reqwest) internally calls open/read/etc on sockets and
// TLS cert files. Without this guard, those calls would re-enter our hooks
// and deadlock or infinite-loop. The thread-local flag ensures that once
// we're inside a hook, nested libc calls pass straight through to the real
// implementation.

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

struct ReentrancyGuard;

impl ReentrancyGuard {
    /// Try to enter the hook. Returns None if we're already inside one
    /// (i.e., this is a nested call from the HTTP client).
    fn try_enter() -> Option<Self> {
        IN_HOOK.with(|flag| {
            if flag.get() {
                None  // Already in a hook — let the call pass through to libc
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
// Config — lazily initialized from environment variables on first use
// ---------------------------------------------------------------------------
// If REEVO_API_URL is not set, CONFIG is None and the shim is effectively
// disabled: every call falls through to the real libc.

const DEFAULT_PREFIX: &str = "/reevofs";

struct Config {
    /// Path prefix to intercept (default: "/reevofs")
    prefix: String,
    /// API namespace, maps to the first path segment in the API URL (default: "skills")
    namespace: String,
    /// API scope, maps to the second path segment (default: "org")
    scope: String,
    /// HTTP client with auth credentials pre-configured
    client: ReevoClient,
}

static CONFIG: Lazy<Option<Config>> = Lazy::new(|| {
    // Re-entrancy guard needed here because reading env vars and building
    // the HTTP client may trigger open() calls on /proc/self/environ, etc.
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

/// Fast prefix check using the compile-time default. Used as a pre-filter
/// before acquiring the re-entrancy guard and doing the full config lookup.
fn quick_prefix_match(path_str: &str) -> bool {
    path_str.starts_with(DEFAULT_PREFIX)
}

/// Check if path matches the configured prefix. Returns the config and the
/// remaining path after the prefix (e.g., "/SKILL.md" from "/reevofs/SKILL.md").
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
// dlsym — resolve the real libc function to forward non-reevofs calls to
// ---------------------------------------------------------------------------

/// Look up the next symbol in the dynamic linker chain (i.e., the real libc
/// implementation of the function we're shadowing).
unsafe fn dlsym_next(name: &[u8]) -> *mut c_void {
    libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const c_char)
}

/// Set errno for the calling thread (platform-specific).
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
// ---------------------------------------------------------------------------
// memfd_create creates an anonymous file backed by RAM. The key advantage
// is that fstat() on this FD returns the correct st_size, and lseek/mmap
// work as expected — the caller can't tell it's not a real file.
//
// Falls back to a pipe if memfd is unavailable (older kernels), though
// pipe FDs don't support lseek or fstat st_size.

fn create_fd_with_content(content: &[u8]) -> c_int {
    unsafe {
        let name = b"reevofs\0";
        // Use raw syscall for memfd_create — the libc wrapper may not exist
        // on all glibc versions, but the kernel syscall is available since 3.17.
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
            // Write all content to the memfd
            let mut written = 0usize;
            while written < content.len() {
                let n = libc::write(fd, content[written..].as_ptr() as *const c_void, content.len() - written);
                if n < 0 { break; }
                written += n as usize;
            }
            // Seek back to start so the caller reads from the beginning
            libc::lseek(fd, 0, libc::SEEK_SET);
            return fd;
        }

        // Fallback: pipe (read end is returned, write end is closed after filling)
        let mut fds = [0 as c_int; 2];
        if libc::pipe(fds.as_mut_ptr()) == 0 {
            let mut written = 0usize;
            while written < content.len() {
                let n = libc::write(fds[1], content[written..].as_ptr() as *const c_void, content.len() - written);
                if n < 0 { break; }
                written += n as usize;
            }
            libc::close(fds[1]); // Close write end; caller reads from fds[0]
            return fds[0];
        }

        -1
    }
}

// ---------------------------------------------------------------------------
// Core open logic — shared by open, open64, openat, openat64
// ---------------------------------------------------------------------------

/// Attempt to serve the path from reevofs. Returns Some(fd) if the path
/// matched /reevofs/*, or None to fall through to real libc open.
fn try_open_reevofs(path_str: &str) -> Option<c_int> {
    if !quick_prefix_match(path_str) {
        return None; // Fast reject — not a /reevofs path
    }
    let _guard = ReentrancyGuard::try_enter()?; // Prevent re-entrancy from HTTP client
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
            Some(-1) // Return error FD — file not found on API
        }
    }
}

// ---------------------------------------------------------------------------
// openat — the primary open syscall used by modern glibc and CPython
// ---------------------------------------------------------------------------
// Python's built-in open() → os.open() → libc openat(AT_FDCWD, path, ...).
// Same for cat, most GNU coreutils, and Node.js.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            // Only intercept absolute paths — relative paths can't be /reevofs/*
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
// open / open64 — legacy variants still used by some programs
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
// ---------------------------------------------------------------------------
// fstat is NOT hooked — since we return real memfd kernel FDs from open(),
// fstat() on those FDs returns correct size/mode natively from the kernel.
//
// For path-based stat (before the file is opened), we synthesize a stat
// buffer by fetching the file content to get its size, or checking if the
// path is a directory via the list_dir API endpoint.

fn try_stat_reevofs(path_str: &str, buf: *mut libc::stat) -> Option<c_int> {
    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, api_path) = match_path(path_str)?;

    // Zero out the stat buffer to avoid returning garbage in unused fields
    unsafe { std::ptr::write_bytes(buf, 0, 1); }

    // Try reading as a file first
    if let Ok(resp) = cfg.client.read_file(&cfg.namespace, &cfg.scope, api_path) {
        unsafe {
            (*buf).st_mode = libc::S_IFREG | 0o644; // Regular file, rw-r--r--
            (*buf).st_size = resp.content.len() as libc::off_t;
            (*buf).st_nlink = 1;
        }
        return Some(0);
    }
    // If not a file, check if it's a directory
    if cfg.client.list_dir(&cfg.namespace, &cfg.scope, api_path).is_ok() {
        unsafe {
            (*buf).st_mode = libc::S_IFDIR | 0o755; // Directory, rwxr-xr-x
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

/// glibc internal stat wrapper used on older glibc versions.
/// The `ver` parameter is the stat version (usually _STAT_VER = 1).
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

/// glibc legacy stat wrapper (pre-glibc 2.33)
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

/// glibc legacy lstat wrapper (pre-glibc 2.33)
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
// access / faccessat — used by shells and runtimes to check file existence
// ---------------------------------------------------------------------------
// For /reevofs/ paths we always return success (0) since the shim can't
// easily check file existence without a full HTTP round-trip. The actual
// open() call will fail with ENOENT if the file doesn't exist.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if quick_prefix_match(s) {
                return 0; // Optimistically report accessible
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
                return 0; // Optimistically report accessible
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"faccessat\0"));
    real(dirfd, path, mode, flags)
}

// ---------------------------------------------------------------------------
// 64-bit variants — CPython on aarch64 glibc 2.33+ uses these instead of
// the non-suffixed versions. Each needs its own dlsym fallback to the
// corresponding *64 symbol to avoid circular calls through the non-64 hooks.
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
