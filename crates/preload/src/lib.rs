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
//!          (fstat/read/lseek all work natively on it)
//! ```
//!
//! ## Strategy
//!
//! - **Read**: fetch content via HTTP, write to memfd, return real kernel FD.
//! - **Write**: open empty memfd, let writes go natively, flush to API on close/dup2.
//! - **Dir**: pre-fetch listing, serve via getdents64 from an in-memory buffer.
//! - **Cache**: lock-free papaya HashMap with 5s TTL, write-through invalidation.
//!
//! ## Hooked functions
//!
//! Covers POSIX, glibc legacy, 64-bit variants, and raw syscall():
//! - File I/O: `open`, `openat`, `open64`, `openat64`, `close`,
//!   `fopen`, `fopen64`, `freopen`, `freopen64`
//! - Stat: `stat`, `lstat`, `fstat`, `fstatat`, `statx`, `__xstat`, `__lxstat`,
//!   `__fxstat`, `__fxstatat`, `stat64`, `lstat64`, `fstatat64`
//! - Access: `access`, `faccessat`, `euidaccess`
//! - Dir: `opendir`, `readdir`, `closedir`, `scandir`, `scandir64`
//! - Write: `write` (memfd passthrough), `dup`, `dup2`, `dup3` (tracking + flush)
//! - Mutate: `unlink`, `unlinkat`, `rmdir`, `mkdir`, `mkdirat`,
//!   `rename`, `renameat`, `renameat2`
//! - Raw: `syscall()` hook for SYS_close, SYS_statx, SYS_openat, SYS_getdents64,
//!   SYS_newfstatat, SYS_renameat2 (catches libuv/Node.js bypassing PLT)
//!
//! Namespaces and permissions are hardcoded in `build_namespaces()`.

#![allow(unsafe_op_in_unsafe_fn)]
#![allow(unused_variables)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
        // Use try_with to handle TLS destruction during process teardown.
        // After TLS is destroyed, try_with returns Err and we return None
        // (treat as re-entrant / skip hook), which is safe.
        IN_HOOK.try_with(|flag| {
            if flag.get() {
                None
            } else {
                flag.set(true);
                Some(ReentrancyGuard)
            }
        }).ok().flatten()
    }
}

impl Drop for ReentrancyGuard {
    fn drop(&mut self) {
        let _ = IN_HOOK.try_with(|flag| flag.set(false));
    }
}

// ---------------------------------------------------------------------------
// Constructor / Destructor — survive fork+exec for redirects
// ---------------------------------------------------------------------------

/// File-based IPC for passing Write-fd mappings across exec.
/// Written by dup2 hook in the child (after fork, before exec), read by
/// the constructor in the exec'd process.
/// Format: "fd:namespace:scope:path\n" per line.
const WFD_DIR: &str = "/tmp/.reevofs_wfd";

fn wfd_path() -> String {
    format!("{}/{}", WFD_DIR, unsafe { libc::getpid() })
}

/// Library constructor — runs when the .so is loaded (including after exec).
/// Restores Write fd tracking from the file written before exec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __reevofs_init() {
    restore_wfd_from_file();
}

// Ensure __reevofs_init runs as a constructor via .init_array.
#[used]
#[unsafe(link_section = ".init_array")]
static INIT: unsafe extern "C" fn() = __reevofs_init;

/// Restore Write fd mappings from the /tmp/.reevofs_wfd/{pid} file.
unsafe fn restore_wfd_from_file() {
    let path = wfd_path();
    let Ok(contents) = std::fs::read_to_string(&path) else { return };
    // Remove the file immediately so it's not inherited by further children.
    let _ = std::fs::remove_file(&path);

    if let Ok(mut map) = FD_MAP.try_lock() {
        for line in contents.lines() {
            let parts: Vec<&str> = line.splitn(4, ':').collect();
            if parts.len() == 4 {
                if let Ok(fd) = parts[0].parse::<c_int>() {
                    map.insert(fd, FdState::Write {
                        namespace: parts[1].to_string(),
                        scope: parts[2].to_string(),
                        path: parts[3].to_string(),
                    });
                }
            }
        }
    }
}

/// Write current Write fd mappings to /tmp/.reevofs_wfd/{pid}.
/// Called from dup2 when propagating Write tracking, so exec'd processes
/// can restore tracking and flush on exit.
fn sync_wfd_to_file() {
    // Collect data inside the lock, then do I/O outside to avoid
    // deadlock with our close hook (which also tries FD_MAP.try_lock).
    let lines: Vec<String> = {
        let Ok(map) = FD_MAP.try_lock() else { return };
        map.iter()
            .filter_map(|(fd, state)| match state {
                FdState::Write { namespace, scope, path } =>
                    Some(format!("{fd}:{namespace}:{scope}:{path}")),
                _ => None,
            })
            .collect()
    };
    let path = wfd_path();
    if lines.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let _ = std::fs::create_dir_all(WFD_DIR);
        let _ = std::fs::write(&path, lines.join("\n"));
    }
}

/// Library destructor — clean up WFD temp files.
/// The fclose hook handles flushing Write fds; this just removes any
/// leftover temp files from sync_wfd_to_file().
unsafe extern "C" fn __reevofs_fini() {
    // Clean up any remaining WFD temp file.
    let path = wfd_path();
    let _ = std::fs::remove_file(&path);
}

#[used]
#[unsafe(link_section = ".fini_array")]
static FINI: unsafe extern "C" fn() = __reevofs_fini;

// ---------------------------------------------------------------------------
// Config — hardcoded mount table
// ---------------------------------------------------------------------------

const PREFIX: &str = "/reevofs/";

#[derive(Clone, Copy, PartialEq)]
enum Access {
    ReadOnly,
    ReadWrite,
}

struct Namespace {
    scope: String,
    access: Access,
}

struct Config {
    client: ReevoClient,
    namespaces: HashMap<String, Namespace>,
}

/// Hardcoded mount table. Only scope values come from env vars.
/// Each namespace is optional — missing env var skips that namespace.
/// Returns None if no namespaces are configured — disables the shim entirely.
fn build_namespaces() -> Option<HashMap<String, Namespace>> {
    let mut ns = HashMap::new();

    if let Ok(scope) = std::env::var("REEVOFS_SCOPE_skills") {
        ns.insert("skills".into(), Namespace {
            scope,
            access: Access::ReadOnly,
        });
    }

    if let Ok(scope) = std::env::var("REEVOFS_SCOPE_output") {
        ns.insert("output".into(), Namespace {
            scope,
            access: Access::ReadWrite,
        });
    }

    if ns.is_empty() {
        return None;
    }
    Some(ns)
}

/// Debug logging — enable with REEVOFS_DEBUG=1
static DEBUG: Lazy<bool> = Lazy::new(|| std::env::var("REEVOFS_DEBUG").is_ok());

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if *DEBUG {
            eprintln!("[reevofs] {}", format!($($arg)*));
        }
    };
}

static CONFIG: Lazy<Option<Config>> = Lazy::new(|| {
    let _guard = ReentrancyGuard::try_enter();

    let api_url = std::env::var("REEVO_API_URL").ok()?;
    let token = std::env::var("REEVO_API_TOKEN").unwrap_or_default();
    let user_id = std::env::var("REEVO_USER_ID").ok();
    let org_id = std::env::var("REEVO_ORG_ID").ok();
    let namespaces = build_namespaces()?;

    Some(Config {
        client: ReevoClient::with_ids(&api_url, &token, user_id.as_deref(), org_id.as_deref()),
        namespaces,
    })
});

fn quick_prefix_match(path_str: &str) -> bool {
    path_str.starts_with(PREFIX)
}

fn is_root_path(path_str: &str) -> bool {
    path_str == "/reevofs" || path_str == "/reevofs/"
}

/// Returns true if any path segment is "..".
fn has_path_traversal(path: &str) -> bool {
    path.split('/').any(|seg| seg == "..")
}

/// Parse /reevofs/{namespace}/{path} → (config, namespace_cfg, namespace, file_path).
/// Returns None if path doesn't match, namespace not in mount table, or path traversal detected.
fn match_path(path_str: &str) -> Option<(&'static Config, &'static Namespace, &str, &str)> {
    let cfg = CONFIG.as_ref()?;
    let rest = path_str.strip_prefix(PREFIX)?;
    let (namespace, file_path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };
    if namespace.is_empty() || has_path_traversal(file_path) {
        return None;
    }
    let ns_cfg = cfg.namespaces.get(namespace)?;
    Some((cfg, ns_cfg, namespace, file_path))
}

// ---------------------------------------------------------------------------
// API response cache — lock-free via papaya
// ---------------------------------------------------------------------------

const CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Clone)]
enum CacheEntry {
    File { content: String, at: Instant },
    Dir { entries: Vec<(String, bool)>, at: Instant },
    NotFound { at: Instant },
}

impl CacheEntry {
    fn is_valid(&self) -> bool {
        let at = match self {
            CacheEntry::File { at, .. } => at,
            CacheEntry::Dir { at, .. } => at,
            CacheEntry::NotFound { at } => at,
        };
        at.elapsed() < CACHE_TTL
    }
}

type CacheKey = (String, String, String); // (namespace, scope, path)

static CACHE: Lazy<papaya::HashMap<CacheKey, CacheEntry>> =
    Lazy::new(papaya::HashMap::new);

fn cache_key(ns: &str, scope: &str, path: &str) -> CacheKey {
    (ns.into(), scope.into(), path.into())
}

fn parent_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(pos) => &path[..pos],
    }
}

/// Invalidate cache entries for a path and its parent directory.
fn invalidate_path(ns: &str, scope: &str, path: &str) {
    let guard = CACHE.pin();
    guard.remove(&cache_key(ns, scope, path));
    guard.remove(&cache_key(ns, scope, parent_path(path)));
}

/// Result of an existence check (avoids cloning full file content for stat/access).
enum ExistsResult {
    IsFile { size: usize },
    IsDir { entries: Vec<(String, bool)> },
    NotFound,
}

/// Check if a file or directory exists, using cache. For stat/access paths.
fn cached_exists(cfg: &Config, ns: &str, scope: &str, path: &str) -> ExistsResult {
    let key = cache_key(ns, scope, path);
    let guard = CACHE.pin();
    if let Some(entry) = guard.get(&key) {
        if entry.is_valid() {
            let result = match entry {
                CacheEntry::File { content, .. } => ExistsResult::IsFile { size: content.len() },
                CacheEntry::Dir { entries, .. } => ExistsResult::IsDir { entries: entries.clone() },
                CacheEntry::NotFound { .. } => ExistsResult::NotFound,
            };
            debug_log!("cached_exists ns={} path={} -> {} (cached)", ns, path, match &result {
                ExistsResult::IsFile { size } => format!("IsFile({})", size),
                ExistsResult::IsDir { entries } => format!("IsDir({})", entries.len()),
                ExistsResult::NotFound => "NotFound".into(),
            });
            return result;
        }
    }
    drop(guard);

    // Cache miss — query API.
    if let Ok(resp) = cfg.client.read_file(ns, scope, path) {
        let size = resp.content.len();
        debug_log!("cached_exists ns={} path={} -> IsFile({}) (read_file hit)", ns, path, size);
        CACHE.pin().insert(key, CacheEntry::File {
            content: resp.content,
            at: Instant::now(),
        });
        return ExistsResult::IsFile { size };
    }
    if let Ok(resp) = cfg.client.list_dir(ns, scope, path) {
        let entries: Vec<(String, bool)> = resp.entries.iter()
            .map(|e| (e.name.clone(), e.is_directory))
            .collect();
        debug_log!("cached_exists ns={} path={} -> list_dir returned {} entries", ns, path, entries.len());
        // Only classify as directory if it actually has children.
        // The real API may return 200 with empty entries for non-existent paths,
        // which would cause stat() to report files as directories (breaking cp/mv).
        if !entries.is_empty() {
            let result = entries.clone();
            CACHE.pin().insert(key, CacheEntry::Dir { entries, at: Instant::now() });
            return ExistsResult::IsDir { entries: result };
        }
    }
    // Namespace roots (api_path == "/") are always valid directories,
    // even when the API returns 404 (no files yet). Without this,
    // `stat /reevofs/output/` fails and tools like cp/mv can't resolve
    // the destination directory.
    if path == "/" {
        debug_log!("cached_exists ns={} path={} -> IsDir(0) (namespace root)", ns, path);
        CACHE.pin().insert(key, CacheEntry::Dir { entries: vec![], at: Instant::now() });
        return ExistsResult::IsDir { entries: vec![] };
    }
    debug_log!("cached_exists ns={} path={} -> NotFound", ns, path);
    CACHE.pin().insert(key, CacheEntry::NotFound { at: Instant::now() });
    ExistsResult::NotFound
}

/// Read file content, using cache.
fn cached_read_file(cfg: &Config, ns: &str, scope: &str, path: &str) -> Result<String, ()> {
    let key = cache_key(ns, scope, path);
    let guard = CACHE.pin();
    if let Some(entry) = guard.get(&key) {
        if entry.is_valid() {
            return match entry {
                CacheEntry::File { content, .. } => Ok(content.clone()),
                CacheEntry::NotFound { .. } => Err(()),
                _ => Err(()), // Dir entry in a read_file path — treat as not-a-file
            };
        }
    }
    drop(guard);

    match cfg.client.read_file(ns, scope, path) {
        Ok(resp) => {
            let content = resp.content;
            CACHE.pin().insert(key, CacheEntry::File {
                content: content.clone(),
                at: Instant::now(),
            });
            Ok(content)
        }
        Err(_) => {
            CACHE.pin().insert(key, CacheEntry::NotFound { at: Instant::now() });
            Err(())
        }
    }
}

/// List directory, using cache.
fn cached_list_dir(cfg: &Config, ns: &str, scope: &str, path: &str) -> Result<Vec<(String, bool)>, ()> {
    let key = cache_key(ns, scope, path);
    let guard = CACHE.pin();
    if let Some(entry) = guard.get(&key) {
        if entry.is_valid() {
            return match entry {
                CacheEntry::Dir { entries, .. } => Ok(entries.clone()),
                CacheEntry::NotFound { .. } => Err(()),
                _ => Err(()), // File entry in a list_dir path
            };
        }
    }
    drop(guard);

    match cfg.client.list_dir(ns, scope, path) {
        Ok(resp) => {
            let entries: Vec<(String, bool)> = resp.entries.iter()
                .map(|e| (e.name.clone(), e.is_directory))
                .collect();
            // Empty entries on a non-root path means the path doesn't exist as
            // a directory.  Some backends return 200 with [] for any path; treating
            // that as a valid dir tricks `cp`/`mv` (which probe with O_DIRECTORY)
            // into appending the source basename → nested path bug.
            if entries.is_empty() && path != "/" {
                debug_log!("cached_list_dir ns={} path={} -> empty entries, treating as NotFound", ns, path);
                CACHE.pin().insert(key, CacheEntry::NotFound { at: Instant::now() });
                return Err(());
            }
            debug_log!("cached_list_dir ns={} path={} -> {} entries", ns, path, entries.len());
            let result = entries.clone();
            CACHE.pin().insert(key, CacheEntry::Dir { entries, at: Instant::now() });
            Ok(result)
        }
        Err(_) => {
            CACHE.pin().insert(key, CacheEntry::NotFound { at: Instant::now() });
            Err(())
        }
    }
}

// ---------------------------------------------------------------------------
// FD tracking — maps kernel FDs to reevofs state
// ---------------------------------------------------------------------------

enum FdState {
    /// Directory listing — entries for readdir, serialized buffer for getdents64.
    Directory {
        base_path: String,
        /// (name, is_directory) entries including "." and "..".
        entries: Vec<(String, bool)>,
        /// Index for readdir() — advances one entry per call.
        readdir_idx: usize,
        /// Pre-serialized linux_dirent64 buffer for getdents64().
        #[allow(dead_code)]
        dirent_buf: Vec<u8>,
        /// Byte offset into dirent_buf for getdents64().
        #[allow(dead_code)]
        getdents_offset: usize,
    },
    /// Write-open FD — writes go to the memfd natively; flushed to API on close.
    Write {
        namespace: String,
        scope: String,
        path: String,
    },
}

static FD_MAP: Lazy<Mutex<HashMap<c_int, FdState>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Look up the base path for a tracked directory FD (for dirfd-relative resolution).
fn get_dir_fd_base(fd: c_int) -> Option<String> {
    let map = FD_MAP.try_lock().ok()?;
    match map.get(&fd)? {
        FdState::Directory { base_path, .. } => Some(base_path.clone()),
        _ => None,
    }
}

/// Resolve a potentially relative path against a dirfd.
/// Returns the full absolute path if the dirfd is a tracked directory FD.
fn resolve_dirfd(dirfd: c_int, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None; // absolute — caller handles it
    }
    let base = get_dir_fd_base(dirfd)?;
    Some(format!("{}/{}", base.trim_end_matches('/'), rel))
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
// Create kernel FDs
// ---------------------------------------------------------------------------

/// Create a memfd (or pipe fallback). Returns a valid kernel FD or -1.
fn create_empty_fd() -> c_int {
    unsafe {
        let name = b"reevofs\0";
        #[cfg(target_os = "linux")]
        {
            #[cfg(target_arch = "x86_64")]
            const SYS_MEMFD: libc::c_long = 319;
            #[cfg(target_arch = "aarch64")]
            const SYS_MEMFD: libc::c_long = 279;
            let fd = libc::syscall(SYS_MEMFD, name.as_ptr(), 0 as c_int) as c_int;
            if fd >= 0 {
                return fd;
            }
        }
        // Fallback: pipe, close write end.
        let mut fds = [0 as c_int; 2];
        if libc::pipe(fds.as_mut_ptr()) == 0 {
            // Call real close — our close hook will pass through (fds[1] not in FD_MAP).
            type CloseF = unsafe extern "C" fn(c_int) -> c_int;
            let real_close: CloseF = std::mem::transmute(dlsym_next(b"close\0"));
            real_close(fds[1]);
            return fds[0];
        }
        -1
    }
}

/// Create a memfd pre-filled with content. Seeked to position 0.
fn create_fd_with_content(content: &[u8]) -> c_int {
    unsafe {
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
            raw_write_all(fd, content);
            libc::lseek(fd, 0, libc::SEEK_SET);
            return fd;
        }

        // Fallback: pipe
        let mut fds = [0 as c_int; 2];
        if libc::pipe(fds.as_mut_ptr()) == 0 {
            raw_write_all(fds[1], content);
            type CloseF = unsafe extern "C" fn(c_int) -> c_int;
            let real_close: CloseF = std::mem::transmute(dlsym_next(b"close\0"));
            real_close(fds[1]);
            return fds[0];
        }
        -1
    }
}

/// Write all bytes to an fd using the real write syscall.
unsafe fn raw_write_all(fd: c_int, data: &[u8]) {
    type WriteF = unsafe extern "C" fn(c_int, *const c_void, libc::size_t) -> libc::ssize_t;
    let real_write: WriteF = std::mem::transmute(dlsym_next(b"write\0"));
    let mut written = 0usize;
    while written < data.len() {
        let n = real_write(fd, data[written..].as_ptr() as *const c_void, data.len() - written);
        if n < 0 { break; }
        written += n as usize;
    }
}

/// Open a real directory FD (backed by an existing dir like "/").
/// This is needed because glibc's opendir() fstats the fd internally (via inline
/// syscall, not through our hooks) and rejects it if it's not S_IFDIR.
/// By borrowing a real directory fd, the kernel-level fstat returns S_IFDIR.
/// Our getdents64 hook intercepts reads and serves the virtual entries instead.
fn create_dir_fd() -> c_int {
    unsafe {
        type F = unsafe extern "C" fn(c_int, *const c_char, c_int, libc::mode_t) -> c_int;
        let real_openat: F = std::mem::transmute(dlsym_next(b"openat\0"));
        real_openat(libc::AT_FDCWD, b"/\0".as_ptr() as *const c_char,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0)
    }
}

// ---------------------------------------------------------------------------
// dirent64 serialization (Linux only)
// ---------------------------------------------------------------------------

/// Serialize a list of (name, is_directory) entries into a linux_dirent64 buffer.
/// Includes "." and ".." as the first two entries.
#[cfg(target_os = "linux")]
fn serialize_dirent64(entries: &[(String, bool)]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut ino: u64 = 1000;

    for (name, is_dir) in entries {
        let name_bytes = name.as_bytes();
        // struct linux_dirent64: d_ino(8) + d_off(8) + d_reclen(2) + d_type(1) + d_name(variable+nul)
        let reclen = (8 + 8 + 2 + 1 + name_bytes.len() + 1 + 7) & !7; // align to 8
        let d_off = (buf.len() + reclen) as u64;
        let d_type: u8 = if *is_dir { 4 } else { 8 }; // DT_DIR / DT_REG

        buf.extend_from_slice(&ino.to_ne_bytes());               // d_ino
        buf.extend_from_slice(&d_off.to_ne_bytes());              // d_off
        buf.extend_from_slice(&(reclen as u16).to_ne_bytes());    // d_reclen
        buf.push(d_type);                                         // d_type
        buf.extend_from_slice(name_bytes);                        // d_name
        buf.push(0);                                              // nul terminator
        while buf.len() < d_off as usize {
            buf.push(0);                                          // padding
        }
        ino += 1;
    }
    buf
}

#[cfg(not(target_os = "linux"))]
fn serialize_dirent64(_entries: &[(String, bool)]) -> Vec<u8> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// Core open logic
// ---------------------------------------------------------------------------

fn try_open_reevofs(path_str: &str, flags: c_int) -> Option<c_int> {
    // ── Root mount point (/reevofs or /reevofs/) ──
    if is_root_path(path_str) {
        if (flags & (libc::O_WRONLY | libc::O_RDWR)) != 0 {
            set_errno(libc::EACCES);
            return Some(-1);
        }
        let _guard = ReentrancyGuard::try_enter()?;
        let cfg = CONFIG.as_ref()?;
        let mut entries: Vec<(String, bool)> = vec![
            (".".into(), true),
            ("..".into(), true),
        ];
        for name in cfg.namespaces.keys() {
            entries.push((name.clone(), true));
        }
        let dirent_buf = serialize_dirent64(&entries);
        let fd = create_dir_fd();
        if fd < 0 {
            set_errno(libc::EIO);
            return Some(-1);
        }
        if let Ok(mut map) = FD_MAP.lock() {
            map.insert(fd, FdState::Directory {
                base_path: "/reevofs".into(),
                entries: entries.clone(),
                readdir_idx: 0,
                dirent_buf,
                getdents_offset: 0,
            });
        }
        return Some(fd);
    }

    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, ns_cfg, namespace, api_path) = match_path(path_str)?;

    let wants_write = (flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC)) != 0;
    let is_dir_open = (flags & libc::O_DIRECTORY) != 0
        || api_path == "/"
        || api_path.ends_with('/');

    // ── Directory open (for ls / readdir) ──
    if is_dir_open {
        // Namespace roots (api_path == "/") always exist as directories,
        // even if the API returns 404 (empty namespace). This matches how
        // the real API works — configured namespaces are always valid dirs.
        let list = match cached_list_dir(cfg, namespace, &ns_cfg.scope, api_path) {
            Ok(entries) => entries,
            Err(_) if api_path == "/" => vec![], // namespace root: empty dir
            Err(_) => {
                set_errno(libc::ENOENT);
                return Some(-1);
            }
        };
        let mut entries: Vec<(String, bool)> = vec![
            (".".into(), true),
            ("..".into(), true),
        ];
        for (name, is_dir) in &list {
            entries.push((name.clone(), *is_dir));
        }
        let dirent_buf = serialize_dirent64(&entries);
        let fd = create_dir_fd();
        if fd < 0 {
            set_errno(libc::EIO);
            return Some(-1);
        }
        // Reconstruct the base path for dirfd-relative resolution.
        let base_path = format!("/reevofs/{}{}", namespace, api_path.trim_end_matches('/'));
        if let Ok(mut map) = FD_MAP.lock() {
            map.insert(fd, FdState::Directory {
                base_path,
                entries: entries.clone(),
                readdir_idx: 0,
                dirent_buf,
                getdents_offset: 0,
            });
        }
        return Some(fd);
    }

    // ── Write open ──
    if wants_write {
        if ns_cfg.access != Access::ReadWrite {
            set_errno(libc::EACCES);
            return Some(-1);
        }
        // If not truncating, pre-fill with existing content so O_RDWR / O_APPEND work.
        let fd = if (flags & libc::O_TRUNC) != 0 || (flags & libc::O_CREAT) != 0 {
            // Check if file exists when only O_CREAT (no O_TRUNC) — fetch existing content.
            if (flags & libc::O_TRUNC) == 0 {
                match cached_read_file(cfg, namespace, &ns_cfg.scope, api_path) {
                    Ok(content) => create_fd_with_content(content.as_bytes()),
                    Err(_) => create_empty_fd(), // new file
                }
            } else {
                create_empty_fd()
            }
        } else {
            // O_WRONLY without O_CREAT/O_TRUNC — file must exist.
            match cached_read_file(cfg, namespace, &ns_cfg.scope, api_path) {
                Ok(content) => create_fd_with_content(content.as_bytes()),
                Err(_) => {
                    set_errno(libc::ENOENT);
                    return Some(-1);
                }
            }
        };
        if fd < 0 {
            set_errno(libc::EIO);
            return Some(-1);
        }
        if let Ok(mut map) = FD_MAP.lock() {
            map.insert(fd, FdState::Write {
                namespace: namespace.into(),
                scope: ns_cfg.scope.clone(),
                path: api_path.into(),
            });
        }
        return Some(fd);
    }

    // ── Read open (default) ──
    match cached_read_file(cfg, namespace, &ns_cfg.scope, api_path) {
        Ok(content) => {
            let fd = create_fd_with_content(content.as_bytes());
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
// Core stat logic
// ---------------------------------------------------------------------------

/// Fill common stat fields that tools like `ls -l` need to display properly.
unsafe fn fill_stat_common(buf: *mut libc::stat) {
    (*buf).st_uid = libc::getuid();
    (*buf).st_gid = libc::getgid();
    (*buf).st_blksize = 4096;
    (*buf).st_blocks = 0;
    // Use a stable fake device/inode so tools don't get confused.
    (*buf).st_dev = 0x52_45_45_56; // "REEV"
    (*buf).st_ino = 1;
    let now = libc::time(std::ptr::null_mut());
    (*buf).st_atime = now;
    (*buf).st_mtime = now;
    (*buf).st_ctime = now;
}

fn try_stat_reevofs(path_str: &str, buf: *mut libc::stat) -> Option<c_int> {
    // ── Root mount point ──
    if is_root_path(path_str) {
        let _guard = ReentrancyGuard::try_enter()?;
        CONFIG.as_ref()?;
        unsafe {
            std::ptr::write_bytes(buf, 0, 1);
            (*buf).st_mode = libc::S_IFDIR | 0o555;
            (*buf).st_nlink = 2;
            fill_stat_common(buf);
        }
        return Some(0);
    }

    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, ns_cfg, namespace, api_path) = match_path(path_str)?;

    unsafe { std::ptr::write_bytes(buf, 0, 1); }

    let file_mode = if ns_cfg.access == Access::ReadWrite {
        libc::S_IFREG | 0o644
    } else {
        libc::S_IFREG | 0o444
    };
    let dir_mode = if ns_cfg.access == Access::ReadWrite {
        libc::S_IFDIR | 0o755
    } else {
        libc::S_IFDIR | 0o555
    };

    match cached_exists(cfg, namespace, &ns_cfg.scope, api_path) {
        ExistsResult::IsFile { size } => {
            unsafe {
                (*buf).st_mode = file_mode;
                (*buf).st_size = size as libc::off_t;
                (*buf).st_nlink = 1;
                fill_stat_common(buf);
            }
            Some(0)
        }
        ExistsResult::IsDir { .. } => {
            unsafe {
                (*buf).st_mode = dir_mode;
                (*buf).st_nlink = 2;
                fill_stat_common(buf);
            }
            Some(0)
        }
        ExistsResult::NotFound => {
            set_errno(libc::ENOENT);
            Some(-1)
        }
    }
}

// ---------------------------------------------------------------------------
// Core statx logic (Linux only — glibc 2.33+ routes stat() through statx)
// ---------------------------------------------------------------------------

/// Fill common statx fields for proper display by tools like `ls -l`.
#[cfg(target_os = "linux")]
unsafe fn fill_statx_common(buf: *mut libc::statx) {
    (*buf).stx_uid = libc::getuid();
    (*buf).stx_gid = libc::getgid();
    (*buf).stx_blksize = 4096;
    (*buf).stx_dev_major = 0x52; // "R"
    (*buf).stx_dev_minor = 0x45; // "E"
    (*buf).stx_ino = 1;
    let now = libc::time(std::ptr::null_mut());
    let mut ts: libc::statx_timestamp = std::mem::zeroed();
    ts.tv_sec = now;
    ts.tv_nsec = 0;
    (*buf).stx_atime = ts;
    (*buf).stx_mtime = ts;
    (*buf).stx_ctime = ts;
    (*buf).stx_btime = ts;
}

#[cfg(target_os = "linux")]
fn try_statx_reevofs(path_str: &str, mask: libc::c_uint, buf: *mut libc::statx) -> Option<c_int> {
    // ── Root mount point ──
    if is_root_path(path_str) {
        let _guard = ReentrancyGuard::try_enter()?;
        CONFIG.as_ref()?;
        unsafe {
            std::ptr::write_bytes(buf, 0, 1);
            (*buf).stx_mask = libc::STATX_BASIC_STATS;
            (*buf).stx_mode = (libc::S_IFDIR | 0o555) as u16;
            (*buf).stx_nlink = 2;
            fill_statx_common(buf);
        }
        return Some(0);
    }

    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, ns_cfg, namespace, api_path) = match_path(path_str)?;

    unsafe { std::ptr::write_bytes(buf, 0, 1); }

    let file_mode: u16 = if ns_cfg.access == Access::ReadWrite {
        (libc::S_IFREG | 0o644) as u16
    } else {
        (libc::S_IFREG | 0o444) as u16
    };
    let dir_mode: u16 = if ns_cfg.access == Access::ReadWrite {
        (libc::S_IFDIR | 0o755) as u16
    } else {
        (libc::S_IFDIR | 0o555) as u16
    };

    match cached_exists(cfg, namespace, &ns_cfg.scope, api_path) {
        ExistsResult::IsFile { size } => {
            unsafe {
                (*buf).stx_mask = libc::STATX_BASIC_STATS;
                (*buf).stx_mode = file_mode;
                (*buf).stx_size = size as u64;
                (*buf).stx_nlink = 1;
                fill_statx_common(buf);
            }
            Some(0)
        }
        ExistsResult::IsDir { .. } => {
            unsafe {
                (*buf).stx_mask = libc::STATX_BASIC_STATS;
                (*buf).stx_mode = dir_mode;
                (*buf).stx_nlink = 2;
                fill_statx_common(buf);
            }
            Some(0)
        }
        ExistsResult::NotFound => {
            set_errno(libc::ENOENT);
            Some(-1)
        }
    }
}

// ---------------------------------------------------------------------------
// Core access logic
// ---------------------------------------------------------------------------

fn try_access_reevofs(path_str: &str, mode: c_int) -> Option<c_int> {
    if is_root_path(path_str) {
        let _guard = ReentrancyGuard::try_enter()?;
        CONFIG.as_ref()?;
        if (mode & libc::W_OK) != 0 {
            set_errno(libc::EACCES);
            return Some(-1);
        }
        return Some(0);
    }

    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, ns_cfg, namespace, api_path) = match_path(path_str)?;

    // Write permission check.
    if (mode & libc::W_OK) != 0 && ns_cfg.access != Access::ReadWrite {
        set_errno(libc::EACCES);
        return Some(-1);
    }

    // Namespace root always exists.
    if api_path == "/" {
        return Some(0);
    }

    // Verify the file or directory actually exists (cached).
    match cached_exists(cfg, namespace, &ns_cfg.scope, api_path) {
        ExistsResult::IsFile { .. } | ExistsResult::IsDir { .. } => Some(0),
        ExistsResult::NotFound => {
            set_errno(libc::ENOENT);
            Some(-1)
        }
    }
}

// ---------------------------------------------------------------------------
// Core unlink logic
// ---------------------------------------------------------------------------

fn try_unlink_reevofs(path_str: &str, flags: c_int) -> Option<c_int> {
    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, ns_cfg, namespace, api_path) = match_path(path_str)?;
    if ns_cfg.access != Access::ReadWrite {
        set_errno(libc::EACCES);
        return Some(-1);
    }
    match cfg.client.delete_file(namespace, &ns_cfg.scope, api_path) {
        Ok(_) => {
            invalidate_path(namespace, &ns_cfg.scope, api_path);
            Some(0)
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
                if let Some(result) = try_open_reevofs(s, flags) {
                    return result;
                }
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_open_reevofs(&full, flags) {
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
            if let Some(result) = try_open_reevofs(s, flags) {
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
// fopen / fopen64 — many coreutils (md5sum, sed, sort, file) use fopen() which
// internally calls __openat_nocancel (inline syscall), bypassing our openat hook.
// Intercept fopen to route /reevofs/ paths through our shim.
// ---------------------------------------------------------------------------

/// Convert fopen mode string to open flags.
fn fopen_mode_to_flags(mode: &str) -> c_int {
    match mode.trim_end_matches('b').trim_end_matches(',').trim_end_matches('e') {
        "r" => libc::O_RDONLY,
        "w" => libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
        "a" => libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
        "r+" => libc::O_RDWR,
        "w+" => libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
        "a+" => libc::O_RDWR | libc::O_CREAT | libc::O_APPEND,
        _ => -1,
    }
}

unsafe fn do_fopen(path: *const c_char, mode: *const c_char, sym: &[u8]) -> *mut libc::FILE {
    if !path.is_null() && !mode.is_null() {
        if let (Ok(path_str), Ok(mode_str)) = (
            CStr::from_ptr(path).to_str(),
            CStr::from_ptr(mode).to_str(),
        ) {
            let flags = fopen_mode_to_flags(mode_str);
            if flags >= 0 {
                if let Some(fd) = try_open_reevofs(path_str, flags) {
                    if fd < 0 {
                        return std::ptr::null_mut();
                    }
                    // Wrap the fd in a FILE* using fdopen.
                    type FdOpenF = unsafe extern "C" fn(c_int, *const c_char) -> *mut libc::FILE;
                    let real_fdopen: FdOpenF = std::mem::transmute(dlsym_next(b"fdopen\0"));
                    let file = real_fdopen(fd, mode);
                    if file.is_null() {
                        // fdopen failed — close the fd.
                        type CloseF = unsafe extern "C" fn(c_int) -> c_int;
                        let real_close: CloseF = std::mem::transmute(dlsym_next(b"close\0"));
                        real_close(fd);
                    }
                    return file;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut libc::FILE;
    let real: F = std::mem::transmute(dlsym_next(sym));
    real(path, mode)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> *mut libc::FILE {
    do_fopen(path, mode, b"fopen\0")
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fopen64(path: *const c_char, mode: *const c_char) -> *mut libc::FILE {
    do_fopen(path, mode, b"fopen64\0")
}

// ---------------------------------------------------------------------------
// freopen / freopen64 — GNU coreutils `uniq` uses freopen() to redirect stdin
// to the input file. Override to route /reevofs/ paths through our shim.
// ---------------------------------------------------------------------------

unsafe fn do_freopen(path: *const c_char, mode: *const c_char, stream: *mut libc::FILE, sym: &[u8]) -> *mut libc::FILE {
    if !path.is_null() && !mode.is_null() && !stream.is_null() {
        if let (Ok(path_str), Ok(mode_str)) = (
            CStr::from_ptr(path).to_str(),
            CStr::from_ptr(mode).to_str(),
        ) {
            let flags = fopen_mode_to_flags(mode_str);
            if flags >= 0 {
                if let Some(fd) = try_open_reevofs(path_str, flags) {
                    if fd < 0 {
                        return std::ptr::null_mut();
                    }
                    // dup2 the new fd onto the stream's underlying fd so the FILE*
                    // (e.g. stdin) remains valid and the caller can keep reading it.
                    let old_fd = libc::fileno(stream);
                    if old_fd >= 0 {
                        libc::dup2(fd, old_fd);
                        libc::close(fd);
                        // Reset the FILE* internal state so it re-reads from the new fd.
                        libc::fflush(stream);
                        libc::rewind(stream);
                        return stream;
                    }
                    // Fallback: close stream, fdopen the new fd.
                    type FcloseF = unsafe extern "C" fn(*mut libc::FILE) -> c_int;
                    let real_fclose: FcloseF = std::mem::transmute(dlsym_next(b"fclose\0"));
                    real_fclose(stream);
                    type FdOpenF = unsafe extern "C" fn(c_int, *const c_char) -> *mut libc::FILE;
                    let real_fdopen: FdOpenF = std::mem::transmute(dlsym_next(b"fdopen\0"));
                    let file = real_fdopen(fd, mode);
                    if file.is_null() {
                        libc::close(fd);
                    }
                    return file;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, *const c_char, *mut libc::FILE) -> *mut libc::FILE;
    let real: F = std::mem::transmute(dlsym_next(sym));
    real(path, mode, stream)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn freopen(path: *const c_char, mode: *const c_char, stream: *mut libc::FILE) -> *mut libc::FILE {
    do_freopen(path, mode, stream, b"freopen\0")
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn freopen64(path: *const c_char, mode: *const c_char, stream: *mut libc::FILE) -> *mut libc::FILE {
    do_freopen(path, mode, stream, b"freopen64\0")
}

// ---------------------------------------------------------------------------
// opendir / closedir — glibc's opendir uses __openat_nocancel (inline syscall)
// which bypasses our openat hook. Override opendir directly.
// ---------------------------------------------------------------------------

/// Common opendir logic: if path matches reevofs, open a real dir fd ("/"),
/// register virtual entries in FD_MAP, then wrap with real fdopendir.
unsafe fn try_opendir_reevofs(path_str: &str) -> Option<*mut libc::DIR> {
    if !is_root_path(path_str) && !quick_prefix_match(path_str) {
        return None;
    }
    // Create the directory fd and register in FD_MAP (reuses try_open_reevofs logic).
    let fd = try_open_reevofs(path_str, libc::O_RDONLY | libc::O_DIRECTORY)?;
    if fd < 0 {
        return Some(std::ptr::null_mut());
    }
    // Wrap in a DIR* using real fdopendir.
    type FdOpenDirF = unsafe extern "C" fn(c_int) -> *mut libc::DIR;
    let real_fdopendir: FdOpenDirF = std::mem::transmute(dlsym_next(b"fdopendir\0"));
    let dir = real_fdopendir(fd);
    if dir.is_null() {
        // fdopendir failed — clean up.
        type CloseF = unsafe extern "C" fn(c_int) -> c_int;
        let real_close: CloseF = std::mem::transmute(dlsym_next(b"close\0"));
        FD_MAP.try_lock().ok().map(|mut map| map.remove(&fd));
        real_close(fd);
    }
    Some(dir)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn opendir(path: *const c_char) -> *mut libc::DIR {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_opendir_reevofs(s) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char) -> *mut libc::DIR;
    let real: F = std::mem::transmute(dlsym_next(b"opendir\0"));
    real(path)
}

// ---------------------------------------------------------------------------
// readdir / readdir64 — glibc's readdir uses __getdents64 (inline syscall),
// bypassing our getdents64 hook. Override readdir directly.
// ---------------------------------------------------------------------------

// Thread-local buffer for returning dirent entries from readdir.
// dirent64: d_ino(8) + d_off(8) + d_reclen(2) + d_type(1) + d_name(256) = 275 → 280 aligned.
thread_local! {
    static READDIR_BUF: std::cell::RefCell<[u8; 280]> = const { std::cell::RefCell::new([0u8; 280]) };
}

unsafe fn try_readdir_reevofs(dirp: *mut libc::DIR) -> Option<*mut libc::dirent> {
    let fd = libc::dirfd(dirp);
    if fd < 0 { return None; }
    let mut map = FD_MAP.try_lock().ok()?;
    let state = map.get_mut(&fd)?;
    match state {
        FdState::Directory { entries, readdir_idx, .. } => {
            if *readdir_idx >= entries.len() {
                return Some(std::ptr::null_mut()); // EOF
            }
            let (ref name, is_dir) = entries[*readdir_idx];
            let ino = (1000 + *readdir_idx) as u64;
            *readdir_idx += 1;

            READDIR_BUF.with(|buf| {
                let mut b = buf.borrow_mut();
                b.fill(0);
                let d_type: u8 = if is_dir { 4 } else { 8 };
                let name_bytes = name.as_bytes();
                let name_len = name_bytes.len().min(255);

                // Write fields into the buffer at the right offsets.
                // struct dirent { d_ino: u64, d_off: u64, d_reclen: u16, d_type: u8, d_name: [c_char; 256] }
                let reclen: u16 = 19 + name_len as u16 + 1; // min size, doesn't need alignment for readdir
                b[0..8].copy_from_slice(&ino.to_ne_bytes());
                b[8..16].copy_from_slice(&(*readdir_idx as u64).to_ne_bytes()); // d_off = position after this entry
                b[16..18].copy_from_slice(&reclen.to_ne_bytes());
                b[18] = d_type;
                b[19..19 + name_len].copy_from_slice(&name_bytes[..name_len]);
                b[19 + name_len] = 0;

                Some(b.as_ptr() as *mut libc::dirent)
            })
        }
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn readdir(dirp: *mut libc::DIR) -> *mut libc::dirent {
    if let Some(result) = try_readdir_reevofs(dirp) {
        return result;
    }
    type F = unsafe extern "C" fn(*mut libc::DIR) -> *mut libc::dirent;
    let real: F = std::mem::transmute(dlsym_next(b"readdir\0"));
    real(dirp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn readdir64(dirp: *mut libc::DIR) -> *mut libc::dirent {
    // On 64-bit Linux, dirent and dirent64 are identical.
    if let Some(result) = try_readdir_reevofs(dirp) {
        return result;
    }
    type F = unsafe extern "C" fn(*mut libc::DIR) -> *mut libc::dirent;
    let real: F = std::mem::transmute(dlsym_next(b"readdir64\0"));
    real(dirp)
}

// ---------------------------------------------------------------------------
// scandir64 — libuv (Node.js) calls scandir64() for fs.readdirSync().
// glibc's scandir internally uses __openat_nocancel (inline syscall) which
// bypasses all our hooks. Override scandir64 directly.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scandir64(
    dirp: *const c_char,
    namelist: *mut *mut *mut libc::dirent,
    filter: Option<unsafe extern "C" fn(*const libc::dirent) -> c_int>,
    compar: Option<unsafe extern "C" fn(*mut *const libc::dirent, *mut *const libc::dirent) -> c_int>,
) -> c_int {
    if !dirp.is_null() {
        if let Ok(path_str) = CStr::from_ptr(dirp).to_str() {
            if is_root_path(path_str) || quick_prefix_match(path_str) {
                if let Some(entries) = try_scandir_reevofs(path_str, filter) {
                    // Allocate dirent** array and fill it.
                    let count = entries.len();
                    let array = libc::malloc(count * std::mem::size_of::<*mut libc::dirent>())
                        as *mut *mut libc::dirent;
                    if array.is_null() {
                        set_errno(libc::ENOMEM);
                        return -1;
                    }
                    for (i, (name, is_dir)) in entries.iter().enumerate() {
                        let name_bytes = name.as_bytes();
                        let name_len = name_bytes.len().min(255);
                        // Allocate a dirent struct (must be freeable with free()).
                        let reclen = 19 + name_len + 1; // d_ino(8) + d_off(8) + d_reclen(2) + d_type(1) + name + NUL
                        let entry = libc::malloc(reclen) as *mut u8;
                        if entry.is_null() {
                            // Free already-allocated entries.
                            for j in 0..i {
                                libc::free(*array.add(j) as *mut c_void);
                            }
                            libc::free(array as *mut c_void);
                            set_errno(libc::ENOMEM);
                            return -1;
                        }
                        std::ptr::write_bytes(entry, 0, reclen);
                        let ino = (1000 + i) as u64;
                        let off = (i + 1) as u64;
                        let d_type: u8 = if *is_dir { 4 } else { 8 };
                        entry.cast::<u64>().write_unaligned(ino);          // d_ino
                        entry.add(8).cast::<u64>().write_unaligned(off);   // d_off
                        entry.add(16).cast::<u16>().write_unaligned(reclen as u16); // d_reclen
                        *entry.add(18) = d_type;                            // d_type
                        std::ptr::copy_nonoverlapping(name_bytes.as_ptr(), entry.add(19), name_len);
                        *entry.add(19 + name_len) = 0;                     // NUL terminator
                        *array.add(i) = entry as *mut libc::dirent;
                    }
                    // Sort if comparator provided.
                    if let Some(cmp) = compar {
                        let slice = std::slice::from_raw_parts_mut(array, count);
                        slice.sort_by(|a, b| {
                            let ap = a as *const *mut libc::dirent as *mut *const libc::dirent;
                            let bp = b as *const *mut libc::dirent as *mut *const libc::dirent;
                            cmp(ap, bp).cmp(&0)
                        });
                    }
                    *namelist = array;
                    return count as c_int;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(
        *const c_char, *mut *mut *mut libc::dirent,
        Option<unsafe extern "C" fn(*const libc::dirent) -> c_int>,
        Option<unsafe extern "C" fn(*mut *const libc::dirent, *mut *const libc::dirent) -> c_int>,
    ) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"scandir64\0"));
    real(dirp, namelist, filter, compar)
}

#[cfg(target_os = "linux")]
fn try_scandir_reevofs(
    path_str: &str,
    filter: Option<unsafe extern "C" fn(*const libc::dirent) -> c_int>,
) -> Option<Vec<(String, bool)>> {
    let _guard = ReentrancyGuard::try_enter()?;

    // Get the namespace listing from the API.
    let entries: Vec<(String, bool)>;

    if is_root_path(path_str) {
        // Root mount: list namespace names.
        let cfg = CONFIG.as_ref()?;
        entries = cfg.namespaces.keys()
            .map(|k| (k.clone(), true))
            .collect();
    } else {
        let (cfg, ns_cfg, namespace, api_path) = match_path(path_str)?;
        entries = cached_list_dir(cfg, namespace, &ns_cfg.scope, api_path).ok()?;
    }

    // Apply filter if provided.
    let mut result = Vec::new();
    for (name, is_dir) in &entries {
        if let Some(f) = filter {
            // Build a temporary dirent for the filter.
            let mut buf = [0u8; 280];
            let name_bytes = name.as_bytes();
            let name_len = name_bytes.len().min(255);
            let d_type: u8 = if *is_dir { 4 } else { 8 };
            buf[18] = d_type;
            buf[19..19 + name_len].copy_from_slice(&name_bytes[..name_len]);
            let accept = unsafe { f(buf.as_ptr() as *const libc::dirent) };
            if accept == 0 {
                continue;
            }
        }
        result.push((name.clone(), *is_dir));
    }
    Some(result)
}

// Also override scandir (non-64) as an alias.
#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scandir(
    dirp: *const c_char,
    namelist: *mut *mut *mut libc::dirent,
    filter: Option<unsafe extern "C" fn(*const libc::dirent) -> c_int>,
    compar: Option<unsafe extern "C" fn(*mut *const libc::dirent, *mut *const libc::dirent) -> c_int>,
) -> c_int {
    // On 64-bit Linux, dirent and dirent64 are identical.
    scandir64(dirp, namelist, filter, compar)
}

// ---------------------------------------------------------------------------
// stat / lstat / fstatat
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatat(dirfd: c_int, path: *const c_char, buf: *mut libc::stat, flag: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if s.starts_with('/') {
                if let Some(result) = try_stat_reevofs(s, buf) {
                    return result;
                }
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_stat_reevofs(&full, buf) {
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
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_stat_reevofs(&full, buf) {
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
// statx — glibc 2.33+ routes stat()/fstatat() through statx on modern kernels
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn statx(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mask: libc::c_uint,
    buf: *mut libc::statx,
) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            // AT_EMPTY_PATH with empty string = fstat on dirfd.
            // glibc's fstat() uses statx(fd, "", AT_EMPTY_PATH, ...) on modern kernels.
            if s.is_empty() && (flags & libc::AT_EMPTY_PATH) != 0 {
                if let Some(result) = try_fstatx_reevofs(dirfd, buf) {
                    return result;
                }
            } else if s.starts_with('/') {
                if let Some(result) = try_statx_reevofs(s, mask, buf) {
                    return result;
                }
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_statx_reevofs(&full, mask, buf) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, libc::c_uint, *mut libc::statx) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"statx\0"));
    real(dirfd, path, flags, mask, buf)
}

// ---------------------------------------------------------------------------
// fstat — tracked FDs need correct st_mode (memfds report S_IFREG, but
// directory FDs must report S_IFDIR or ls/opendir will reject them).
// ---------------------------------------------------------------------------

fn try_fstat_reevofs(fd: c_int, buf: *mut libc::stat) -> Option<c_int> {
    let map = FD_MAP.try_lock().ok()?;
    let state = map.get(&fd)?;
    unsafe { std::ptr::write_bytes(buf, 0, 1); }
    match state {
        FdState::Directory { .. } => {
            unsafe {
                (*buf).st_mode = libc::S_IFDIR | 0o555;
                (*buf).st_nlink = 2;
            }
            Some(0)
        }
        FdState::Write { .. } => {
            // Let real fstat handle the memfd — it knows the actual size.
            None
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstat(fd: c_int, buf: *mut libc::stat) -> c_int {
    if let Some(result) = try_fstat_reevofs(fd, buf) {
        return result;
    }
    type F = unsafe extern "C" fn(c_int, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"fstat\0"));
    real(fd, buf)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __fxstat(ver: c_int, fd: c_int, buf: *mut libc::stat) -> c_int {
    if let Some(result) = try_fstat_reevofs(fd, buf) {
        return result;
    }
    type F = unsafe extern "C" fn(c_int, c_int, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"__fxstat\0"));
    real(ver, fd, buf)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstat64(fd: c_int, buf: *mut libc::stat) -> c_int {
    if let Some(result) = try_fstat_reevofs(fd, buf) {
        return result;
    }
    type F = unsafe extern "C" fn(c_int, *mut libc::stat) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"fstat64\0"));
    real(fd, buf)
}

#[cfg(target_os = "linux")]
fn try_fstatx_reevofs(fd: c_int, buf: *mut libc::statx) -> Option<c_int> {
    let map = FD_MAP.try_lock().ok()?;
    let state = map.get(&fd)?;
    unsafe { std::ptr::write_bytes(buf, 0, 1); }
    match state {
        FdState::Directory { .. } => {
            unsafe {
                (*buf).stx_mask = libc::STATX_BASIC_STATS;
                (*buf).stx_mode = (libc::S_IFDIR | 0o555) as u16;
                (*buf).stx_nlink = 2;
                (*buf).stx_blksize = 4096;
            }
            Some(0)
        }
        FdState::Write { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// access / faccessat
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_access_reevofs(s, mode) {
                return result;
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
            if s.starts_with('/') {
                if let Some(result) = try_access_reevofs(s, mode) {
                    return result;
                }
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_access_reevofs(&full, mode) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"faccessat\0"));
    real(dirfd, path, mode, flags)
}

// euidaccess — GNU coreutils (sort, etc.) call euidaccess() before opening files.
// euidaccess internally does newfstatat, bypassing our hooks. Override it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn euidaccess(path: *const c_char, mode: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_access_reevofs(s, mode) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"euidaccess\0"));
    real(path, mode)
}

// ---------------------------------------------------------------------------
// Flush helper — reads memfd content via the fd and sends to API.
// The fd must still be open and valid.
// ---------------------------------------------------------------------------

/// Returns true if flush succeeded (or there was nothing to flush).
unsafe fn flush_write_fd(fd: c_int, namespace: &str, scope: &str, path: &str) -> bool {
    libc::lseek(fd, 0, libc::SEEK_SET);
    let mut content = Vec::new();
    let mut tmp = [0u8; 8192];
    type ReadF = unsafe extern "C" fn(c_int, *mut c_void, libc::size_t) -> libc::ssize_t;
    let real_read: ReadF = std::mem::transmute(dlsym_next(b"read\0"));
    loop {
        let n = real_read(fd, tmp.as_mut_ptr() as *mut c_void, tmp.len());
        if n <= 0 { break; }
        content.extend_from_slice(&tmp[..n as usize]);
    }
    if let Some(_guard) = ReentrancyGuard::try_enter() {
        if let Some(cfg) = CONFIG.as_ref() {
            let text = String::from_utf8_lossy(&content);
            match cfg.client.write_file(namespace, scope, path, &text) {
                Ok(_) => {
                    invalidate_path(namespace, scope, path);
                    return true;
                }
                Err(_) => return false,
            }
        }
    }
    true // No config or reentrancy — nothing to flush
}

/// Same as flush_write_fd but skips invalidate_path.
/// Used by the fclose hook which runs during process teardown (atexit);
/// accessing papaya's CACHE during teardown triggers a TLS panic.
unsafe fn flush_write_fd_no_invalidate(fd: c_int, namespace: &str, scope: &str, path: &str) -> bool {
    libc::lseek(fd, 0, libc::SEEK_SET);
    let mut content = Vec::new();
    let mut tmp = [0u8; 8192];
    type ReadF = unsafe extern "C" fn(c_int, *mut c_void, libc::size_t) -> libc::ssize_t;
    let real_read: ReadF = std::mem::transmute(dlsym_next(b"read\0"));
    loop {
        let n = real_read(fd, tmp.as_mut_ptr() as *mut c_void, tmp.len());
        if n <= 0 { break; }
        content.extend_from_slice(&tmp[..n as usize]);
    }
    if let Some(_guard) = ReentrancyGuard::try_enter() {
        if let Some(cfg) = CONFIG.as_ref() {
            let text = String::from_utf8_lossy(&content);
            return cfg.client.write_file(namespace, scope, path, &text).is_ok();
        }
    }
    true
}

// ---------------------------------------------------------------------------
// dup / dup2 / dup3 — propagate FD tracking so bash's redirect pattern works.
// bash does: fd=open(file); dup2(fd,1); close(fd); echo writes to 1; dup2(saved,1)
// Without dup2 tracking, the echo content is never flushed to the API.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dup(oldfd: c_int) -> c_int {
    type F = unsafe extern "C" fn(c_int) -> c_int;
    let real_dup: F = std::mem::transmute(dlsym_next(b"dup\0"));
    let newfd = real_dup(oldfd);
    if newfd >= 0 {
        if let Ok(mut map) = FD_MAP.try_lock() {
            if let Some(FdState::Write { namespace, scope, path }) = map.get(&oldfd) {
                let cloned = FdState::Write {
                    namespace: namespace.clone(),
                    scope: scope.clone(),
                    path: path.clone(),
                };
                map.insert(newfd, cloned);
            }
        }
    }
    newfd
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dup2(oldfd: c_int, newfd: c_int) -> c_int {
    if oldfd == newfd {
        // dup2 with same fd is a no-op (just validates fd is open).
        type F = unsafe extern "C" fn(c_int, c_int) -> c_int;
        let real: F = std::mem::transmute(dlsym_next(b"dup2\0"));
        return real(oldfd, newfd);
    }

    // If newfd is a tracked Write fd, flush its content before dup2 replaces it.
    // This is the critical path: bash's dup2(saved_stdout, 1) triggers this
    // for fd 1 which holds the echo output in the memfd.
    let old_state = FD_MAP.try_lock().ok().and_then(|mut map| map.remove(&newfd));
    if let Some(FdState::Write { namespace, scope, path }) = old_state {
        if !flush_write_fd(newfd, &namespace, &scope, &path) {
            set_errno(libc::EIO);
            return -1;
        }
    }

    // Call real dup2.
    type F = unsafe extern "C" fn(c_int, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"dup2\0"));
    let result = real(oldfd, newfd);

    // Propagate Write tracking from oldfd to newfd.
    if result >= 0 {
        if let Ok(mut map) = FD_MAP.try_lock() {
            if let Some(FdState::Write { namespace, scope, path }) = map.get(&oldfd) {
                let cloned = FdState::Write {
                    namespace: namespace.clone(),
                    scope: scope.clone(),
                    path: path.clone(),
                };
                map.insert(newfd, cloned);
            }
        }
        // Sync to env so exec'd processes can restore tracking.
        sync_wfd_to_file();
    }
    result
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dup3(oldfd: c_int, newfd: c_int, flags: c_int) -> c_int {
    // Same logic as dup2 — flush tracked newfd, then propagate.
    let old_state = FD_MAP.try_lock().ok().and_then(|mut map| map.remove(&newfd));
    if let Some(FdState::Write { namespace, scope, path }) = old_state {
        if !flush_write_fd(newfd, &namespace, &scope, &path) {
            set_errno(libc::EIO);
            return -1;
        }
    }

    type F = unsafe extern "C" fn(c_int, c_int, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"dup3\0"));
    let result = real(oldfd, newfd, flags);

    if result >= 0 {
        if let Ok(mut map) = FD_MAP.try_lock() {
            if let Some(FdState::Write { namespace, scope, path }) = map.get(&oldfd) {
                let cloned = FdState::Write {
                    namespace: namespace.clone(),
                    scope: scope.clone(),
                    path: path.clone(),
                };
                map.insert(newfd, cloned);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// close — flush write FDs to API, clean up tracked FDs
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    // Remove from map first (brief lock, released before any I/O).
    let state = FD_MAP.try_lock().ok().and_then(|mut map| map.remove(&fd));

    if let Some(FdState::Write { namespace, scope, path }) = state {
        // Sync env to remove this fd from exec tracking.
        sync_wfd_to_file();
        if !flush_write_fd(fd, &namespace, &scope, &path) {
            // Close the real FD but report error to caller.
            type CloseF = unsafe extern "C" fn(c_int) -> c_int;
            let real_close: CloseF = std::mem::transmute(dlsym_next(b"close\0"));
            real_close(fd);
            set_errno(libc::EIO);
            return -1;
        }
    }

    // Always call real close.
    type CloseF = unsafe extern "C" fn(c_int) -> c_int;
    let real_close: CloseF = std::mem::transmute(dlsym_next(b"close\0"));
    real_close(fd)
}

// ---------------------------------------------------------------------------
// fclose — glibc's fclose uses __close_nocancel (inline syscall) which
// bypasses our close hook. Hook fclose to flush Write fds before closing.
// Critical for coreutils that call close_stdout() at exit (sort, uniq, etc.).
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fclose(stream: *mut libc::FILE) -> c_int {
    if !stream.is_null() {
        let fd = libc::fileno(stream);
        if fd >= 0 {
            let state = FD_MAP.try_lock().ok().and_then(|mut map| map.remove(&fd));
            if let Some(FdState::Write { namespace, scope, path }) = state {
                sync_wfd_to_file();
                // Flush the stream to ensure all buffered content is in the memfd.
                libc::fflush(stream);
                // Flush fd content to API. Skip invalidate_path because this
                // runs during process exit (via close_stdout atexit handler)
                // and the papaya CACHE's TLS is unsafe to access during teardown.
                flush_write_fd_no_invalidate(fd, &namespace, &scope, &path);
            }
        }
    }
    type F = unsafe extern "C" fn(*mut libc::FILE) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"fclose\0"));
    real(stream)
}

// ---------------------------------------------------------------------------
// getdents64 / getdents — serve directory listings from FD_MAP
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getdents64(fd: c_int, dirp: *mut c_void, count: libc::size_t) -> c_int {
    if let Ok(mut map) = FD_MAP.try_lock() {
        if let Some(FdState::Directory { dirent_buf, getdents_offset: offset, .. }) = map.get_mut(&fd) {
            if *offset >= dirent_buf.len() {
                return 0; // EOF
            }
            let remaining = &dirent_buf[*offset..];
            let mut copied = 0usize;
            while copied < remaining.len() {
                // Need at least 19 bytes to read d_reclen at offset 16.
                if copied + 19 > remaining.len() { break; }
                let reclen = u16::from_ne_bytes([
                    remaining[copied + 16],
                    remaining[copied + 17],
                ]) as usize;
                if copied + reclen > count { break; }
                copied += reclen;
            }
            if copied == 0 {
                // Buffer too small for even one entry.
                set_errno(libc::EINVAL);
                return -1;
            }
            std::ptr::copy_nonoverlapping(remaining.as_ptr(), dirp as *mut u8, copied);
            *offset += copied;
            return copied as c_int;
        }
    }
    // Not our FD — pass through.
    type F = unsafe extern "C" fn(c_int, *mut c_void, libc::size_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"getdents64\0"));
    real(fd, dirp, count)
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getdents(fd: c_int, dirp: *mut c_void, count: libc::size_t) -> c_int {
    // On modern Linux getdents and getdents64 use the same struct layout
    // when compiled with _FILE_OFFSET_BITS=64. Delegate to the same logic.
    if let Ok(mut map) = FD_MAP.try_lock() {
        if let Some(FdState::Directory { dirent_buf, getdents_offset: offset, .. }) = map.get_mut(&fd) {
            if *offset >= dirent_buf.len() {
                return 0;
            }
            let remaining = &dirent_buf[*offset..];
            let mut copied = 0usize;
            while copied < remaining.len() {
                if copied + 19 > remaining.len() { break; }
                let reclen = u16::from_ne_bytes([
                    remaining[copied + 16],
                    remaining[copied + 17],
                ]) as usize;
                if copied + reclen > count { break; }
                copied += reclen;
            }
            if copied == 0 {
                set_errno(libc::EINVAL);
                return -1;
            }
            std::ptr::copy_nonoverlapping(remaining.as_ptr(), dirp as *mut u8, copied);
            *offset += copied;
            return copied as c_int;
        }
    }
    type F = unsafe extern "C" fn(c_int, *mut c_void, libc::size_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"getdents\0"));
    real(fd, dirp, count)
}

// ---------------------------------------------------------------------------
// unlinkat — delete files/dirs on writable namespaces
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if s.starts_with('/') {
                if let Some(result) = try_unlink_reevofs(s, flags) {
                    return result;
                }
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_unlink_reevofs(&full, flags) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"unlinkat\0"));
    real(dirfd, path, flags)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlink(path: *const c_char) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_unlink_reevofs(s, 0) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"unlink\0"));
    real(path)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rmdir(path: *const c_char) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            #[cfg(target_os = "linux")]
            let at_removedir = libc::AT_REMOVEDIR;
            #[cfg(not(target_os = "linux"))]
            let at_removedir = 0x200; // AT_REMOVEDIR on macOS
            if let Some(result) = try_unlink_reevofs(s, at_removedir) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"rmdir\0"));
    real(path)
}

// ---------------------------------------------------------------------------
// mkdirat / mkdir — no-op success on writable namespaces
// (API creates directories implicitly on file write)
// ---------------------------------------------------------------------------

fn try_mkdir_reevofs(path_str: &str) -> Option<c_int> {
    if !quick_prefix_match(path_str) {
        return None;
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (_cfg, ns_cfg, _namespace, _api_path) = match_path(path_str)?;
    if ns_cfg.access != Access::ReadWrite {
        set_errno(libc::EACCES);
        return Some(-1);
    }
    // No-op success — API creates directories implicitly.
    Some(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdirat(dirfd: c_int, path: *const c_char, mode: libc::mode_t) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if s.starts_with('/') {
                if let Some(result) = try_mkdir_reevofs(s) {
                    return result;
                }
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_mkdir_reevofs(&full) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, libc::mode_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"mkdirat\0"));
    real(dirfd, path, mode)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdir(path: *const c_char, mode: libc::mode_t) -> c_int {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if let Some(result) = try_mkdir_reevofs(s) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, libc::mode_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"mkdir\0"));
    real(path, mode)
}

// ---------------------------------------------------------------------------
// Core rename logic — implemented as read→write→delete (no server-side rename API)
// ---------------------------------------------------------------------------

fn try_rename_reevofs(old_path: &str, new_path: &str) -> Option<c_int> {
    let src_reevo = quick_prefix_match(old_path);
    let dst_reevo = quick_prefix_match(new_path);

    // Neither path is under /reevofs/ — not our business.
    if !src_reevo && !dst_reevo {
        return None;
    }

    // Cross-filesystem rename: source outside /reevofs/, dest inside.
    // Return EXDEV so mv falls back to copy (which works via open/write/close).
    if !src_reevo && dst_reevo {
        set_errno(libc::EXDEV);
        return Some(-1);
    }

    // Dest outside /reevofs/ but source inside — also cross-device.
    if src_reevo && !dst_reevo {
        set_errno(libc::EXDEV);
        return Some(-1);
    }
    let _guard = ReentrancyGuard::try_enter()?;
    let (cfg, src_ns_cfg, src_ns, src_api_path) = match_path(old_path)?;
    let (_cfg2, dst_ns_cfg, dst_ns, dst_api_path) = match_path(new_path)?;

    // Source must be readable, destination must be writable.
    if dst_ns_cfg.access != Access::ReadWrite {
        set_errno(libc::EACCES);
        return Some(-1);
    }

    // Read source content.
    let content = match cfg.client.read_file(src_ns, &src_ns_cfg.scope, src_api_path) {
        Ok(resp) => resp.content,
        Err(_) => {
            set_errno(libc::ENOENT);
            return Some(-1);
        }
    };

    // Write to destination.
    if cfg.client.write_file(dst_ns, &dst_ns_cfg.scope, dst_api_path, &content).is_err() {
        set_errno(libc::EIO);
        return Some(-1);
    }
    invalidate_path(dst_ns, &dst_ns_cfg.scope, dst_api_path);

    // Delete source (only if source namespace is writable).
    if src_ns_cfg.access == Access::ReadWrite {
        let _ = cfg.client.delete_file(src_ns, &src_ns_cfg.scope, src_api_path);
        invalidate_path(src_ns, &src_ns_cfg.scope, src_api_path);
    }

    Some(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn renameat2(
    olddirfd: c_int, oldpath: *const c_char,
    newdirfd: c_int, newpath: *const c_char,
    _flags: libc::c_uint,
) -> c_int {
    if !oldpath.is_null() && !newpath.is_null() {
        if let (Ok(old_s), Ok(new_s)) = (
            CStr::from_ptr(oldpath).to_str(),
            CStr::from_ptr(newpath).to_str(),
        ) {
            let old_full = if old_s.starts_with('/') {
                Some(old_s.to_string())
            } else {
                resolve_dirfd(olddirfd, old_s)
            };
            let new_full = if new_s.starts_with('/') {
                Some(new_s.to_string())
            } else {
                resolve_dirfd(newdirfd, new_s)
            };
            if let (Some(ref o), Some(ref n)) = (old_full, new_full) {
                if let Some(result) = try_rename_reevofs(o, n) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, *const c_char, libc::c_uint) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"renameat2\0"));
    real(olddirfd, oldpath, newdirfd, newpath, _flags)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn renameat(
    olddirfd: c_int, oldpath: *const c_char,
    newdirfd: c_int, newpath: *const c_char,
) -> c_int {
    if !oldpath.is_null() && !newpath.is_null() {
        if let (Ok(old_s), Ok(new_s)) = (
            CStr::from_ptr(oldpath).to_str(),
            CStr::from_ptr(newpath).to_str(),
        ) {
            let old_full = if old_s.starts_with('/') {
                Some(old_s.to_string())
            } else {
                resolve_dirfd(olddirfd, old_s)
            };
            let new_full = if new_s.starts_with('/') {
                Some(new_s.to_string())
            } else {
                resolve_dirfd(newdirfd, new_s)
            };
            if let (Some(ref o), Some(ref n)) = (old_full, new_full) {
                if let Some(result) = try_rename_reevofs(o, n) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, *const c_char) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"renameat\0"));
    real(olddirfd, oldpath, newdirfd, newpath)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rename(oldpath: *const c_char, newpath: *const c_char) -> c_int {
    if !oldpath.is_null() && !newpath.is_null() {
        if let (Ok(old_s), Ok(new_s)) = (
            CStr::from_ptr(oldpath).to_str(),
            CStr::from_ptr(newpath).to_str(),
        ) {
            if let Some(result) = try_rename_reevofs(old_s, new_s) {
                return result;
            }
        }
    }
    type F = unsafe extern "C" fn(*const c_char, *const c_char) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"rename\0"));
    real(oldpath, newpath)
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
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_stat_reevofs(&full, buf) {
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
                if let Some(result) = try_open_reevofs(s, flags) {
                    return result;
                }
            } else if let Some(full) = resolve_dirfd(dirfd, s) {
                if let Some(result) = try_open_reevofs(&full, flags) {
                    return result;
                }
            }
        }
    }
    type F = unsafe extern "C" fn(c_int, *const c_char, c_int, libc::mode_t) -> c_int;
    let real: F = std::mem::transmute(dlsym_next(b"openat64\0"));
    real(dirfd, path, flags, mode)
}

// ---------------------------------------------------------------------------
// syscall() hook — intercepts raw syscall() calls that bypass glibc wrappers.
// libuv (Node.js) uses syscall(SYS_close, fd) and syscall(SYS_statx, ...)
// which skip our close/statx hooks entirely. This catches those.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall(
    number: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
    a6: libc::c_long,
) -> libc::c_long {
    type SyscallF = unsafe extern "C" fn(
        libc::c_long, libc::c_long, libc::c_long, libc::c_long,
        libc::c_long, libc::c_long, libc::c_long,
    ) -> libc::c_long;

    // SYS_close — flush tracked write FDs before the kernel closes them.
    if number == libc::SYS_close {
        let fd = a1 as c_int;
        let state = FD_MAP.try_lock().ok().and_then(|mut map| map.remove(&fd));
        if let Some(FdState::Write { namespace, scope, path }) = state {
            flush_write_fd(fd, &namespace, &scope, &path);
        }
        let real: SyscallF = std::mem::transmute(dlsym_next(b"syscall\0"));
        return real(number, a1, a2, a3, a4, a5, a6);
    }

    // SYS_statx — handle virtual paths.
    // syscall(SYS_statx, dirfd, path, flags, mask, buf)
    if number == libc::SYS_statx {
        let dirfd = a1 as c_int;
        let path_ptr = a2 as *const c_char;
        let flags = a3 as c_int;
        let mask = a4 as libc::c_uint;
        let buf = a5 as *mut libc::statx;
        if !path_ptr.is_null() {
            if let Ok(s) = CStr::from_ptr(path_ptr).to_str() {
                if s.is_empty() && (flags & libc::AT_EMPTY_PATH) != 0 {
                    if let Some(result) = try_fstatx_reevofs(dirfd, buf) {
                        return result as libc::c_long;
                    }
                } else if s.starts_with('/') {
                    if let Some(result) = try_statx_reevofs(s, mask, buf) {
                        return result as libc::c_long;
                    }
                } else if let Some(full) = resolve_dirfd(dirfd, s) {
                    if let Some(result) = try_statx_reevofs(&full, mask, buf) {
                        return result as libc::c_long;
                    }
                }
            }
        }
    }

    // SYS_newfstatat — some runtimes use fstatat via raw syscall.
    // syscall(SYS_newfstatat, dirfd, path, buf, flags)
    #[cfg(target_arch = "aarch64")]
    if number == 79 {
        // __NR_newfstatat = 79 on aarch64
        let dirfd = a1 as c_int;
        let path_ptr = a2 as *const c_char;
        let buf = a3 as *mut libc::stat;
        if !path_ptr.is_null() {
            if let Ok(s) = CStr::from_ptr(path_ptr).to_str() {
                if s.starts_with('/') {
                    if let Some(result) = try_stat_reevofs(s, buf) {
                        return result as libc::c_long;
                    }
                } else if let Some(full) = resolve_dirfd(dirfd, s) {
                    if let Some(result) = try_stat_reevofs(&full, buf) {
                        return result as libc::c_long;
                    }
                }
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    if number == 262 {
        // __NR_newfstatat = 262 on x86_64
        let dirfd = a1 as c_int;
        let path_ptr = a2 as *const c_char;
        let buf = a3 as *mut libc::stat;
        if !path_ptr.is_null() {
            if let Ok(s) = CStr::from_ptr(path_ptr).to_str() {
                if s.starts_with('/') {
                    if let Some(result) = try_stat_reevofs(s, buf) {
                        return result as libc::c_long;
                    }
                } else if let Some(full) = resolve_dirfd(dirfd, s) {
                    if let Some(result) = try_stat_reevofs(&full, buf) {
                        return result as libc::c_long;
                    }
                }
            }
        }
    }

    // SYS_openat — libuv uses syscall(SYS_openat, dirfd, path, flags, mode).
    // Intercept to handle virtual directory opens and file opens.
    if number == libc::SYS_openat {
        let dirfd = a1 as c_int;
        let path_ptr = a2 as *const c_char;
        let flags = a3 as c_int;
        if !path_ptr.is_null() {
            if let Ok(s) = CStr::from_ptr(path_ptr).to_str() {
                if s.starts_with('/') {
                    if let Some(result) = try_open_reevofs(s, flags) {
                        return result as libc::c_long;
                    }
                } else if let Some(full) = resolve_dirfd(dirfd, s) {
                    if let Some(result) = try_open_reevofs(&full, flags) {
                        return result as libc::c_long;
                    }
                }
            }
        }
    }

    // SYS_getdents64 — serve directory listings for tracked directory FDs.
    if number == libc::SYS_getdents64 {
        let fd = a1 as c_int;
        let dirp = a2 as *mut c_void;
        let count = a3 as libc::size_t;
        if let Ok(mut map) = FD_MAP.try_lock() {
            if let Some(FdState::Directory { dirent_buf, getdents_offset: offset, .. }) = map.get_mut(&fd) {
                if *offset >= dirent_buf.len() {
                    return 0; // EOF
                }
                let remaining = &dirent_buf[*offset..];
                let mut copied = 0usize;
                while copied < remaining.len() {
                    if copied + 19 > count { break; } // need at least header
                    let reclen = u16::from_ne_bytes([remaining[copied + 16], remaining[copied + 17]]) as usize;
                    if copied + reclen > count { break; }
                    copied += reclen;
                }
                if copied > 0 {
                    std::ptr::copy_nonoverlapping(remaining.as_ptr(), dirp as *mut u8, copied);
                    *offset += copied;
                }
                return copied as libc::c_long;
            }
        }
    }

    // SYS_renameat2 — intercept rename via raw syscall.
    // syscall(SYS_renameat2, olddirfd, oldpath, newdirfd, newpath, flags)
    if number == libc::SYS_renameat2 {
        let olddirfd = a1 as c_int;
        let oldpath_ptr = a2 as *const c_char;
        let newdirfd = a3 as c_int;
        let newpath_ptr = a4 as *const c_char;
        if !oldpath_ptr.is_null() && !newpath_ptr.is_null() {
            if let (Ok(old_s), Ok(new_s)) = (
                CStr::from_ptr(oldpath_ptr).to_str(),
                CStr::from_ptr(newpath_ptr).to_str(),
            ) {
                let old_full = if old_s.starts_with('/') {
                    Some(old_s.to_string())
                } else {
                    resolve_dirfd(olddirfd, old_s)
                };
                let new_full = if new_s.starts_with('/') {
                    Some(new_s.to_string())
                } else {
                    resolve_dirfd(newdirfd, new_s)
                };
                if let (Some(ref o), Some(ref n)) = (old_full, new_full) {
                    if let Some(result) = try_rename_reevofs(o, n) {
                        return result as libc::c_long;
                    }
                }
            }
        }
    }

    let real: SyscallF = std::mem::transmute(dlsym_next(b"syscall\0"));
    real(number, a1, a2, a3, a4, a5, a6)
}
