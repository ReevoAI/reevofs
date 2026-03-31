//! LD_PRELOAD shim that intercepts libc filesystem calls and redirects
//! paths under a configurable prefix (default `/reevofs/`) to the Reevo API.
//!
//! Only intercepts calls where the path starts with the prefix.
//! All other calls pass through to the real libc with zero overhead.
//!
//! Usage:
//!   LD_PRELOAD=/usr/local/lib/libreevofs_preload.so some_command
//!
//! Environment variables:
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
// Config
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

    Some(Config {
        prefix,
        namespace,
        scope,
        client: ReevoClient::with_ids(&api_url, &token, user_id.as_deref(), org_id.as_deref()),
    })
});

/// If path starts with our prefix, strip it and return the API path.
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
// Virtual FD table — uses high positive fds (900_000+) to avoid collisions
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

fn is_virtual_fd(fd: c_int) -> bool {
    fd >= 900_000
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
// Intercepted functions — only touch /reevofs/* paths
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some((cfg, api_path)) = match_path(s) {
                return match cfg.client.read_file(&cfg.namespace, &cfg.scope, api_path) {
                    Ok(resp) => alloc_vfd(resp.content.into_bytes()),
                    Err(_) => { set_errno(libc::ENOENT); -1 }
                };
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, c_int, libc::mode_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"open64\0"));
    real(path, flags, mode)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some((cfg, api_path)) = match_path(s) {
                return match cfg.client.read_file(&cfg.namespace, &cfg.scope, api_path) {
                    Ok(resp) => alloc_vfd(resp.content.into_bytes()),
                    Err(_) => { set_errno(libc::ENOENT); -1 }
                };
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, c_int, libc::mode_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"open\0"));
    real(path, flags, mode)
}

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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some((cfg, api_path)) = match_path(s) {
                std::ptr::write_bytes(buf, 0, 1);
                // Try as file
                if let Ok(resp) = cfg.client.read_file(&cfg.namespace, &cfg.scope, api_path) {
                    (*buf).st_mode = libc::S_IFREG | 0o644;
                    (*buf).st_size = resp.content.len() as libc::off_t;
                    (*buf).st_nlink = 1;
                    return 0;
                }
                // Try as directory
                if cfg.client.list_dir(&cfg.namespace, &cfg.scope, api_path).is_ok() {
                    (*buf).st_mode = libc::S_IFDIR | 0o755;
                    (*buf).st_nlink = 2;
                    return 0;
                }
                set_errno(libc::ENOENT);
                return -1;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"stat\0"));
    real(path, buf)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    stat(path, buf)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if match_path(s).is_some() {
                return 0;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"access\0"));
    real(path, mode)
}
