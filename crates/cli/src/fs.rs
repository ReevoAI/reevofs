//! FUSE filesystem backed by the Reevo API.
//!
//! Mount layout:
//!   /{namespace}/...files...           single-scope namespace (scope injected,
//!                                       matches the LD_PRELOAD shim)
//!   /{namespace}/{scope}/...files...   multi-scope namespace (scope is a
//!                                       navigable subdir; legacy dev fallback)
//!
//! Namespaces and their scopes are configured at runtime via env vars,
//! matching the LD_PRELOAD shim conventions (executor.py):
//!   REEVOFS_SCOPE_skills           e.g. "overlay"
//!   REEVOFS_SCOPE_output           chat_id UUID
//!   REEVOFS_SCOPE_chat_attachments literal "user"
//! Each maps to a single scope, so the namespace directory IS the scope root
//! (e.g. /reevofs/output/report.csv → output/<chat_id>/report.csv) — identical
//! to how the shim injects the scope, so the same paths work under both. A
//! namespace whose env var is unset is skipped (its directory is not mounted).
//! If none of the env vars are set, falls back to the legacy hardcoded
//! /skills/{system,org,user} tree (multi-scope, scope-as-subdir) for dev.
//!
//! Requires the `fuse` feature and macFUSE (macOS) or libfuse (Linux).

#![cfg(feature = "fuse")]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};
use log::{debug, error, info, warn};

use reevofs_api::{ApiError, ReevoClient};

/// Upper bound on `truncate -s N` growth handled in-process. A grow has to
/// allocate `N` bytes in the FUSE daemon and PUT them, so unbounded sizes
/// (e.g. `fallocate -l 10G`) would OOM the mount. 16 MiB covers realistic
/// edit-parity cases (log preallocation, small JSON, SQLite pages) and
/// stops accidental giant grows with EFBIG.
const MAX_TRUNCATE_GROW: u64 = 16 * 1024 * 1024;

/// Directory entry/attr cache TTL. Listings change infrequently enough
/// that 5s gives noticeable readdir speedup without surprising users.
const TTL: Duration = Duration::from_secs(5);
/// File entry/attr cache TTL. MUST be zero. After a write+flush we update
/// the in-memory size, but the kernel won't re-stat within the cache
/// window — so `echo X > f; cat f` returns empty because the kernel
/// still thinks size=0 from the create() reply. Our in-memory lookup is
/// a single hashmap probe, so the cost of zero caching is negligible.
const FILE_TTL: Duration = Duration::ZERO;
const BLOCK_SIZE: u32 = 512;

const ROOT_INO: u64 = 1;

#[derive(Debug, Clone)]
enum InodeKind {
    Root,
    Namespace { name: String },
    Scope { namespace: String, scope: String },
    RemoteDir { namespace: String, scope: String, path: String },
    RemoteFile { namespace: String, scope: String, path: String },
}

#[derive(Debug, Clone)]
struct InodeEntry {
    ino: u64,
    kind: InodeKind,
    children: Vec<u64>,
    content: Option<Vec<u8>>,
    cache_time: Option<SystemTime>,
}

#[derive(Debug)]
struct WriteBuffer {
    namespace: String,
    scope: String,
    path: String,
    data: Vec<u8>,
}

pub struct ReevoFS {
    client: ReevoClient,
    inodes: Mutex<HashMap<u64, InodeEntry>>,
    dir_children: Mutex<HashMap<u64, HashMap<String, u64>>>,
    next_ino: AtomicU64,
    write_buffers: Mutex<HashMap<u64, WriteBuffer>>,
    next_fh: AtomicU64,
    namespaces: Vec<(String, Vec<String>)>,
}

/// Map a backend ApiError to the closest POSIX errno. Forbidden becomes
/// EACCES (not EIO) so sandbox JWT scope violations surface to the caller
/// as "permission denied" rather than a generic I/O error. Conflict
/// becomes EXDEV so `mv` falls back to copy+unlink for cases the rename
/// endpoint declines (directory renames, dest-exists-with-noreplace).
fn api_error_to_errno(e: &ApiError) -> Errno {
    match e {
        ApiError::NotFound => Errno::ENOENT,
        ApiError::Forbidden => Errno::EACCES,
        ApiError::Conflict => Errno::EXDEV,
        ApiError::BadRequest(_) => Errno::EINVAL,
        ApiError::Network(_) => Errno::EIO,
    }
}

/// Build the namespace mount table from REEVOFS_SCOPE_* env vars, mirroring
/// the shim conventions in salestech-be's executor.py. Returns one
/// `(namespace, [scope])` per configured namespace. If no env var is set,
/// returns the legacy hardcoded skills triplet so standalone `reevofs mount`
/// remains usable for dev.
fn load_namespaces_from_env() -> Vec<(String, Vec<String>)> {
    const KNOWN: &[(&str, &str)] = &[
        ("skills", "REEVOFS_SCOPE_skills"),
        ("output", "REEVOFS_SCOPE_output"),
        ("chat_attachments", "REEVOFS_SCOPE_chat_attachments"),
    ];
    let mut configured: Vec<(String, Vec<String>)> = Vec::new();
    for (ns, var) in KNOWN {
        match std::env::var(var) {
            Ok(scope) if !scope.is_empty() => {
                info!("mount: namespace={ns} scope={scope} (from {var})");
                configured.push(((*ns).to_string(), vec![scope]));
            }
            _ => {
                debug!("mount: namespace={ns} skipped (env {var} unset)");
            }
        }
    }
    if configured.is_empty() {
        info!("mount: no REEVOFS_SCOPE_* env vars set; falling back to legacy skills/{{system,org,user}}");
        return vec![(
            "skills".to_string(),
            vec!["system".to_string(), "org".to_string(), "user".to_string()],
        )];
    }
    configured
}

impl ReevoFS {
    pub fn new(client: ReevoClient) -> Self {
        let namespaces = load_namespaces_from_env();

        let mut fs = Self {
            client,
            inodes: Mutex::new(HashMap::new()),
            dir_children: Mutex::new(HashMap::new()),
            next_ino: AtomicU64::new(2),
            next_fh: AtomicU64::new(1),
            write_buffers: Mutex::new(HashMap::new()),
            namespaces,
        };
        fs.init_virtual_tree();
        fs
    }

    fn init_virtual_tree(&mut self) {
        let root = InodeEntry {
            ino: ROOT_INO,
            kind: InodeKind::Root,
            children: Vec::new(),
            content: None,
            cache_time: None,
        };
        self.inodes.get_mut().unwrap().insert(ROOT_INO, root);
        self.dir_children.get_mut().unwrap().insert(ROOT_INO, HashMap::new());

        for (ns_name, scopes) in &self.namespaces {
            let ns_ino = self.alloc_ino();

            // Single-scope namespaces collapse the scope level: the namespace
            // directory IS the scope root, so /reevofs/<ns>/<path> maps directly
            // to (ns, scope, path). This matches the LD_PRELOAD shim, which
            // injects the scope from env rather than exposing it as a navigable
            // subdir. Without this, paths written for the shim (e.g.
            // /reevofs/output/report.csv) resolve their first segment as the
            // *scope* and hit the wrong backend path. The production namespaces
            // (output, skills, chat_attachments) are always single-scope.
            //
            // Multi-scope namespaces (the legacy dev skills/{system,org,user}
            // fallback) keep the explicit scope subdir tree below — that's the
            // only case where a cross-scope rename within a namespace is even
            // expressible and needs a scope dir to be EXDEV against. Cross-
            // namespace renames remain EXDEV either way (namespaces stay
            // separate directories).
            let ns_kind = match scopes.as_slice() {
                [single] => InodeKind::Scope {
                    namespace: ns_name.clone(),
                    scope: single.clone(),
                },
                _ => InodeKind::Namespace { name: ns_name.clone() },
            };
            let ns_entry = InodeEntry {
                ino: ns_ino,
                kind: ns_kind,
                children: Vec::new(),
                content: None,
                cache_time: None,
            };
            self.inodes.get_mut().unwrap().insert(ns_ino, ns_entry);
            self.dir_children.get_mut().unwrap().insert(ns_ino, HashMap::new());

            self.inodes.get_mut().unwrap().get_mut(&ROOT_INO).unwrap().children.push(ns_ino);
            self.dir_children.get_mut().unwrap().get_mut(&ROOT_INO).unwrap().insert(ns_name.clone(), ns_ino);

            // Only multi-scope namespaces expose explicit scope subdirs; for a
            // single-scope namespace the namespace inode (created above as a
            // Scope) already is the scope root.
            if scopes.len() > 1 {
                for scope_name in scopes {
                    let scope_ino = self.alloc_ino();
                    let scope_entry = InodeEntry {
                        ino: scope_ino,
                        kind: InodeKind::Scope {
                            namespace: ns_name.clone(),
                            scope: scope_name.clone(),
                        },
                        children: Vec::new(),
                        content: None,
                        cache_time: None,
                    };
                    self.inodes.get_mut().unwrap().insert(scope_ino, scope_entry);
                    self.dir_children.get_mut().unwrap().insert(scope_ino, HashMap::new());

                    self.inodes.get_mut().unwrap().get_mut(&ns_ino).unwrap().children.push(scope_ino);
                    self.dir_children.get_mut().unwrap().get_mut(&ns_ino).unwrap().insert(scope_name.clone(), scope_ino);
                }
            }
        }
    }

    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    fn dir_attr(ino: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn file_attr(ino: u64, size: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: (size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn populate_children(&self, parent_ino: u64) {
        let inodes = self.inodes.lock().unwrap();
        let entry = match inodes.get(&parent_ino) {
            Some(e) => e.clone(),
            None => return,
        };

        if let Some(cache_time) = entry.cache_time {
            if cache_time.elapsed().unwrap_or(Duration::MAX) < TTL {
                return;
            }
        }

        let (namespace, scope, path) = match &entry.kind {
            InodeKind::Scope { namespace, scope } => {
                (namespace.clone(), scope.clone(), "/".to_string())
            }
            InodeKind::RemoteDir { namespace, scope, path } => {
                (namespace.clone(), scope.clone(), path.clone())
            }
            _ => return,
        };
        drop(inodes);

        let api_entries = match self.client.list_dir(&namespace, &scope, &path) {
            Ok(resp) => resp.entries,
            Err(ApiError::NotFound) => {
                debug!("list_dir not found: {namespace}/{scope}{path}");
                Vec::new()
            }
            Err(e) => {
                warn!("list_dir failed for {namespace}/{scope}{path}: {e}");
                return;
            }
        };

        let mut inodes = self.inodes.lock().unwrap();
        let mut dir_children = self.dir_children.lock().unwrap();

        // Collect existing children first to avoid borrow issues
        let existing: HashMap<String, u64> = dir_children
            .get(&parent_ino)
            .cloned()
            .unwrap_or_default();

        let mut new_children: HashMap<String, u64> = HashMap::new();
        let mut new_child_inos = Vec::new();

        for api_entry in &api_entries {
            if let Some(&existing_ino) = existing.get(&api_entry.name) {
                new_children.insert(api_entry.name.clone(), existing_ino);
                new_child_inos.push(existing_ino);
                continue;
            }

            let child_ino = self.alloc_ino();
            let child_path = if path == "/" {
                format!("/{}", api_entry.name)
            } else {
                format!("{}/{}", path.trim_end_matches('/'), api_entry.name)
            };

            let kind = if api_entry.is_directory {
                InodeKind::RemoteDir {
                    namespace: namespace.clone(),
                    scope: scope.clone(),
                    path: child_path,
                }
            } else {
                InodeKind::RemoteFile {
                    namespace: namespace.clone(),
                    scope: scope.clone(),
                    path: child_path,
                }
            };

            let child_entry = InodeEntry {
                ino: child_ino,
                kind,
                children: Vec::new(),
                content: None,
                cache_time: None,
            };
            inodes.insert(child_ino, child_entry);
            if api_entry.is_directory {
                dir_children.insert(child_ino, HashMap::new());
            }
            new_children.insert(api_entry.name.clone(), child_ino);
            new_child_inos.push(child_ino);
        }

        dir_children.insert(parent_ino, new_children);

        if let Some(parent) = inodes.get_mut(&parent_ino) {
            parent.children = new_child_inos;
            parent.cache_time = Some(SystemTime::now());
        }
    }

    fn fetch_file_content(&self, ino: u64) -> Option<Vec<u8>> {
        let inodes = self.inodes.lock().unwrap();
        let entry = inodes.get(&ino)?.clone();
        drop(inodes);

        if entry.content.is_some() {
            if let Some(cache_time) = entry.cache_time {
                if cache_time.elapsed().unwrap_or(Duration::MAX) < TTL {
                    return entry.content;
                }
            }
        }

        let (namespace, scope, path) = match &entry.kind {
            InodeKind::RemoteFile { namespace, scope, path } => {
                (namespace.clone(), scope.clone(), path.clone())
            }
            _ => return None,
        };

        match self.client.read_file(&namespace, &scope, &path) {
            Ok(data) => {
                let mut inodes = self.inodes.lock().unwrap();
                if let Some(e) = inodes.get_mut(&ino) {
                    e.content = Some(data.clone());
                    e.cache_time = Some(SystemTime::now());
                }
                Some(data)
            }
            Err(e) => {
                warn!("read_file failed for {namespace}/{scope}{path}: {e}");
                None
            }
        }
    }

    fn invalidate_cache(&self, parent_ino: u64) {
        let mut inodes = self.inodes.lock().unwrap();
        if let Some(parent) = inodes.get_mut(&parent_ino) {
            parent.cache_time = None;
        }
    }

    fn resolve_parent(&self, parent: u64) -> Option<(String, String, String)> {
        let inodes = self.inodes.lock().unwrap();
        match inodes.get(&parent).map(|e| &e.kind) {
            Some(InodeKind::Scope { namespace, scope }) => {
                Some((namespace.clone(), scope.clone(), "/".to_string()))
            }
            Some(InodeKind::RemoteDir { namespace, scope, path }) => {
                Some((namespace.clone(), scope.clone(), path.clone()))
            }
            _ => None,
        }
    }

    fn make_child_path(parent_path: &str, name: &str) -> String {
        if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", parent_path.trim_end_matches('/'))
        }
    }
}

impl Filesystem for ReevoFS {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = parent.into();
        let name_str = name.to_string_lossy().to_string();
        debug!("lookup: parent={parent}, name={name_str}");

        self.populate_children(parent);

        let child = {
            let dir_children = self.dir_children.lock().unwrap();
            dir_children.get(&parent).and_then(|c| c.get(&name_str).copied())
        };

        let Some(child_ino) = child else {
            reply.error(Errno::ENOENT);
            return;
        };

        // For RemoteFile entries discovered via _list (no content cached
        // yet), fetch on first lookup so size is accurate. Otherwise the
        // kernel sees size=0 and `cat`/`read()` returns nothing for a
        // file that's never been written through this mount.
        let needs_fetch = {
            let inodes = self.inodes.lock().unwrap();
            matches!(
                inodes.get(&child_ino).map(|e| (&e.kind, &e.content)),
                Some((InodeKind::RemoteFile { .. }, None))
            )
        };
        if needs_fetch {
            self.fetch_file_content(child_ino);
        }

        let inodes = self.inodes.lock().unwrap();
        if let Some(entry) = inodes.get(&child_ino) {
            match &entry.kind {
                InodeKind::RemoteFile { .. } => {
                    let size = entry.content.as_ref().map(|c| c.len() as u64).unwrap_or(0);
                    reply.entry(&FILE_TTL, &Self::file_attr(child_ino, size), Generation(0));
                }
                _ => {
                    reply.entry(&TTL, &Self::dir_attr(child_ino), Generation(0));
                }
            }
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino: u64 = ino.into();
        let inodes = self.inodes.lock().unwrap();
        if let Some(entry) = inodes.get(&ino) {
            match &entry.kind {
                InodeKind::RemoteFile { .. } => {
                    let size = entry.content.as_ref().map(|c| c.len() as u64).unwrap_or(0);
                    reply.attr(&FILE_TTL, &Self::file_attr(ino, size));
                }
                _ => {
                    reply.attr(&TTL, &Self::dir_attr(ino));
                }
            }
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    /// Set file attributes — handles truncate, O_TRUNC, chmod, utimens, chown.
    ///
    /// Only `size` has real effect on the backend (truncate). Other attrs
    /// (mode/uid/gid/atime/mtime) are accepted silently so utilities that
    /// call `chmod`/`utimes` opportunistically (sed, vim, cp -p) don't
    /// abort the surrounding operation. The kernel still gets the
    /// requested values back via ReplyAttr so its attribute cache is
    /// consistent.
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino: u64 = ino.into();
        let fh: Option<u64> = fh.map(|h| h.into());
        debug!("setattr: ino={ino}, size={size:?}, fh={fh:?}");

        let snapshot = {
            let inodes = self.inodes.lock().unwrap();
            inodes
                .get(&ino)
                .map(|e| (e.kind.clone(), e.content.clone()))
        };
        let Some((kind, current_content)) = snapshot else {
            reply.error(Errno::ENOENT);
            return;
        };

        let (namespace, scope, path) = match &kind {
            InodeKind::RemoteFile { namespace, scope, path } => {
                (namespace.clone(), scope.clone(), path.clone())
            }
            // Virtual / directory inodes: accept and return dir_attr so chmod
            // on a directory doesn't fail.
            _ => {
                reply.attr(&TTL, &Self::dir_attr(ino));
                return;
            }
        };

        // Non-size setattr (chmod/chown/utimens) — no-op.
        let Some(new_size) = size else {
            let cur_size = current_content.as_ref().map(|c| c.len() as u64).unwrap_or(0);
            reply.attr(&FILE_TTL, &Self::file_attr(ino, cur_size));
            return;
        };

        if new_size > MAX_TRUNCATE_GROW {
            warn!(
                "setattr: refusing size={new_size} for {namespace}/{scope}{path} (cap {MAX_TRUNCATE_GROW})"
            );
            reply.error(Errno::EFBIG);
            return;
        }

        // If the truncate came from `ftruncate(fd, N)` with an open fd that
        // already has a pending write buffer (e.g. Python r+: write() then
        // truncate()), the kernel-buffered bytes live in our write_buffer
        // map, not in entry.content. PUT-ing entry.content here would be
        // overwritten by the subsequent flush of the (now-stale) buffer.
        // Resize the buffer in place instead and let flush handle the PUT.
        if let Some(fh) = fh {
            let mut buffers = self.write_buffers.lock().unwrap();
            if let Some(buf) = buffers.get_mut(&fh) {
                buf.data.resize(new_size as usize, 0);
                reply.attr(&FILE_TTL, &Self::file_attr(ino, new_size));
                return;
            }
        }

        // No pending buffer — synchronous PUT path. Used by `truncate -s`
        // (no fd), `> file` (kernel-side O_TRUNC pre-open), and similar.
        let current = match current_content {
            Some(c) => c,
            None => match self.client.read_file(&namespace, &scope, &path) {
                Ok(b) => b,
                Err(ApiError::NotFound) => Vec::new(),
                Err(e) => {
                    error!("setattr fetch {namespace}/{scope}{path} failed: {e}");
                    reply.error(api_error_to_errno(&e));
                    return;
                }
            },
        };

        let new_content = if new_size == 0 {
            Vec::new()
        } else if new_size <= current.len() as u64 {
            current[..new_size as usize].to_vec()
        } else {
            let mut v = current;
            v.resize(new_size as usize, 0);
            v
        };

        match self.client.write_file(&namespace, &scope, &path, &new_content) {
            Ok(_) => {
                let final_size = new_content.len() as u64;
                {
                    let mut inodes = self.inodes.lock().unwrap();
                    if let Some(e) = inodes.get_mut(&ino) {
                        e.content = Some(new_content);
                        e.cache_time = Some(SystemTime::now());
                    }
                }
                reply.attr(&FILE_TTL, &Self::file_attr(ino, final_size));
            }
            Err(e) => {
                error!("setattr PUT {namespace}/{scope}{path} failed: {e}");
                reply.error(api_error_to_errno(&e));
            }
        }
    }

    /// Allocate a unique file handle per open so concurrent opens of
    /// different inodes don't collide on fh=0 (the default impl). The
    /// per-fh write buffer is created lazily on the first write — read-only
    /// opens never allocate one.
    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let fh = self.alloc_fh();
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    /// We don't enforce POSIX permissions in the FUSE layer — the backend
    /// is authoritative (sandbox JWT scope, 403 → EACCES at the write
    /// path). Returning ok here lets `test -w`, `access(F_OK)`, and
    /// editors that pre-check writability proceed.
    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    /// Synthetic filesystem stats — there's no underlying block device.
    /// Reports 1 TiB total with most of it free, and 1M inodes free, so
    /// `df` and installers that gate on free-space checks proceed.
    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let bsize: u32 = 4096;
        let blocks: u64 = (1u64 << 40) / bsize as u64; // 1 TiB
        reply.statfs(blocks, blocks, blocks, 1_000_000, 1_000_000, bsize, 255, bsize);
    }

    /// fsync is redundant for us — every `flush` already PUTs bytes to the
    /// backend synchronously, so there are no in-kernel dirty pages we
    /// need to push. Default would return ENOSYS, which makes
    /// safety-conscious apps (vim with `set fsync`, sqlite, atomic-write
    /// libraries) fail their save path.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// Same reasoning as fsync — directory state is whatever the backend
    /// reports; there's nothing to sync client-side.
    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino: u64 = ino.into();
        debug!("readdir: ino={ino}, offset={offset}");

        self.populate_children(ino);

        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ino, FileType::Directory, "..".to_string()),
        ];

        let dir_children = self.dir_children.lock().unwrap();
        if let Some(children) = dir_children.get(&ino) {
            let inodes = self.inodes.lock().unwrap();
            for (name, &child_ino) in children {
                let ft = if let Some(e) = inodes.get(&child_ino) {
                    match &e.kind {
                        InodeKind::RemoteFile { .. } => FileType::RegularFile,
                        _ => FileType::Directory,
                    }
                } else {
                    FileType::Directory
                };
                entries.push((child_ino, ft, name.clone()));
            }
        }

        for (i, (entry_ino, ft, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*entry_ino), (i + 1) as u64, *ft, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino: u64 = ino.into();
        debug!("read: ino={ino}, offset={offset}, size={size}");

        if let Some(content) = self.fetch_file_content(ino) {
            let start = offset as usize;
            if start >= content.len() {
                reply.data(&[]);
            } else {
                let end = (start + size as usize).min(content.len());
                reply.data(&content[start..end]);
            }
        } else {
            reply.error(Errno::EIO);
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let parent: u64 = parent.into();
        let name_str = name.to_string_lossy().to_string();
        debug!("create: parent={parent}, name={name_str}");

        let (namespace, scope, parent_path) = match self.resolve_parent(parent) {
            Some(v) => v,
            None => {
                reply.error(Errno::EACCES);
                return;
            }
        };

        let file_path = Self::make_child_path(&parent_path, &name_str);

        match self.client.write_file(&namespace, &scope, &file_path, &[]) {
            Ok(_) => {
                let child_ino = self.alloc_ino();
                let fh = self.alloc_fh();

                let child_entry = InodeEntry {
                    ino: child_ino,
                    kind: InodeKind::RemoteFile {
                        namespace: namespace.clone(),
                        scope: scope.clone(),
                        path: file_path.clone(),
                    },
                    children: Vec::new(),
                    content: Some(Vec::new()),
                    cache_time: Some(SystemTime::now()),
                };

                {
                    let mut inodes = self.inodes.lock().unwrap();
                    inodes.insert(child_ino, child_entry);
                    if let Some(p) = inodes.get_mut(&parent) {
                        p.children.push(child_ino);
                    }
                }

                self.dir_children.lock().unwrap()
                    .entry(parent)
                    .or_default()
                    .insert(name_str, child_ino);

                self.write_buffers.lock().unwrap().insert(
                    fh,
                    WriteBuffer {
                        namespace,
                        scope,
                        path: file_path,
                        data: Vec::new(),
                    },
                );

                let attr = Self::file_attr(child_ino, 0);
                reply.created(&FILE_TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
            }
            Err(e) => {
                error!("create failed: {e}");
                reply.error(api_error_to_errno(&e));
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let ino: u64 = ino.into();
        let fh: u64 = fh.into();
        let offset = offset as usize;
        debug!("write: ino={ino}, fh={fh}, offset={offset}, len={}", data.len());

        let mut buffers = self.write_buffers.lock().unwrap();
        if let Some(buf) = buffers.get_mut(&fh) {
            let end = offset + data.len();
            if buf.data.len() < end {
                buf.data.resize(end, 0);
            }
            buf.data[offset..end].copy_from_slice(data);
            reply.written(data.len() as u32);
        } else {
            let info = {
                let inodes = self.inodes.lock().unwrap();
                inodes.get(&ino).and_then(|entry| {
                    if let InodeKind::RemoteFile { namespace, scope, path } = &entry.kind {
                        Some((
                            namespace.clone(),
                            scope.clone(),
                            path.clone(),
                            entry.content.clone().unwrap_or_default(),
                        ))
                    } else {
                        None
                    }
                })
            };

            if let Some((namespace, scope, path, mut existing)) = info {
                let end = offset + data.len();
                if existing.len() < end {
                    existing.resize(end, 0);
                }
                existing[offset..end].copy_from_slice(data);

                buffers.insert(
                    fh,
                    WriteBuffer {
                        namespace,
                        scope,
                        path,
                        data: existing,
                    },
                );
                reply.written(data.len() as u32);
            } else {
                reply.error(Errno::EBADF);
            }
        }
    }

    fn flush(&self, _req: &Request, ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        let ino: u64 = ino.into();
        let fh: u64 = fh.into();
        debug!("flush: ino={ino}, fh={fh}");

        let buffers = self.write_buffers.lock().unwrap();
        if let Some(buf) = buffers.get(&fh) {
            let data = buf.data.clone();
            let ns = buf.namespace.clone();
            let scope = buf.scope.clone();
            let path = buf.path.clone();
            drop(buffers);

            match self.client.write_file(&ns, &scope, &path, &data) {
                Ok(_) => {
                    let mut inodes = self.inodes.lock().unwrap();
                    if let Some(e) = inodes.get_mut(&ino) {
                        e.content = Some(data);
                        e.cache_time = Some(SystemTime::now());
                    }
                    reply.ok();
                }
                Err(e) => {
                    error!("flush write_file failed: {e}");
                    reply.error(api_error_to_errno(&e));
                }
            }
        } else {
            reply.ok();
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let ino: u64 = ino.into();
        let fh: u64 = fh.into();
        debug!("release: ino={ino}, fh={fh}");
        self.write_buffers.lock().unwrap().remove(&fh);
        reply.ok();
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent: u64 = parent.into();
        let name_str = name.to_string_lossy().to_string();
        debug!("mkdir: parent={parent}, name={name_str}");

        let (namespace, scope, parent_path) = match self.resolve_parent(parent) {
            Some(v) => v,
            None => {
                reply.error(Errno::EACCES);
                return;
            }
        };

        let dir_path = Self::make_child_path(&parent_path, &name_str);
        let placeholder_path = format!("{}/.keep", dir_path);

        match self.client.write_file(&namespace, &scope, &placeholder_path, &[]) {
            Ok(_) => {
                let child_ino = self.alloc_ino();
                let child_entry = InodeEntry {
                    ino: child_ino,
                    kind: InodeKind::RemoteDir {
                        namespace,
                        scope,
                        path: dir_path,
                    },
                    children: Vec::new(),
                    content: None,
                    cache_time: None,
                };

                {
                    let mut inodes = self.inodes.lock().unwrap();
                    inodes.insert(child_ino, child_entry);
                    if let Some(p) = inodes.get_mut(&parent) {
                        p.children.push(child_ino);
                    }
                }

                {
                    let mut dc = self.dir_children.lock().unwrap();
                    dc.entry(parent).or_default().insert(name_str, child_ino);
                    dc.insert(child_ino, HashMap::new());
                }

                reply.entry(&TTL, &Self::dir_attr(child_ino), Generation(0));
            }
            Err(e) => {
                error!("mkdir failed: {e}");
                reply.error(api_error_to_errno(&e));
            }
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent: u64 = parent.into();
        let name_str = name.to_string_lossy().to_string();
        debug!("unlink: parent={parent}, name={name_str}");

        let child_ino = {
            let dc = self.dir_children.lock().unwrap();
            dc.get(&parent).and_then(|c| c.get(&name_str).copied())
        };

        if let Some(child_ino) = child_ino {
            let inodes = self.inodes.lock().unwrap();
            if let Some(entry) = inodes.get(&child_ino) {
                if let InodeKind::RemoteFile { namespace, scope, path } = &entry.kind {
                    let (ns, sc, p) = (namespace.clone(), scope.clone(), path.clone());
                    drop(inodes);

                    match self.client.delete_file(&ns, &sc, &p) {
                        Ok(_) => {
                            self.inodes.lock().unwrap().remove(&child_ino);
                            if let Some(children) = self.dir_children.lock().unwrap().get_mut(&parent) {
                                children.remove(&name_str);
                            }
                            self.invalidate_cache(parent);
                            reply.ok();
                            return;
                        }
                        Err(e) => {
                            error!("unlink failed: {e}");
                            reply.error(api_error_to_errno(&e));
                            return;
                        }
                    }
                }
            }
        }
        reply.error(Errno::ENOENT);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent: u64 = parent.into();
        let name_str = name.to_string_lossy().to_string();
        debug!("rmdir: parent={parent}, name={name_str}");

        let child_ino = {
            let dc = self.dir_children.lock().unwrap();
            dc.get(&parent).and_then(|c| c.get(&name_str).copied())
        };

        if let Some(child_ino) = child_ino {
            let inodes = self.inodes.lock().unwrap();
            if let Some(entry) = inodes.get(&child_ino) {
                if let InodeKind::RemoteDir { namespace, scope, path } = &entry.kind {
                    let (ns, sc, p) = (namespace.clone(), scope.clone(), path.clone());
                    drop(inodes);

                    match self.client.delete_file(&ns, &sc, &p) {
                        Ok(_) => {
                            self.inodes.lock().unwrap().remove(&child_ino);
                            {
                                let mut dc = self.dir_children.lock().unwrap();
                                dc.remove(&child_ino);
                                if let Some(children) = dc.get_mut(&parent) {
                                    children.remove(&name_str);
                                }
                            }
                            self.invalidate_cache(parent);
                            reply.ok();
                            return;
                        }
                        Err(e) => {
                            error!("rmdir failed: {e}");
                            reply.error(api_error_to_errno(&e));
                            return;
                        }
                    }
                }
            }
        }
        reply.error(Errno::ENOENT);
    }

    /// Rename a file within the same namespace+scope. Calls the BE's
    /// native rename endpoint
    /// (`POST /api/v2/fs/{ns}/{scope}/{path}?op=rename`) so the operation
    /// is atomic at the row level — no byte transfer, no window where
    /// both paths exist, and `created_at` is preserved.
    ///
    /// Cross-namespace and cross-scope renames return EXDEV locally before
    /// the network call so coreutils `mv` falls back to recursive
    /// copy+unlink. Directory renames are surfaced as EXDEV the same way,
    /// either pre-flight (our local check) or via a 409 from the server
    /// mapped to EXDEV in [`api_error_to_errno`].
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent: u64 = parent.into();
        let newparent: u64 = newparent.into();
        let name_str = name.to_string_lossy().to_string();
        let newname_str = newname.to_string_lossy().to_string();
        debug!("rename: {parent}/{name_str} -> {newparent}/{newname_str}");

        let Some((src_ns, src_scope, _src_parent_path)) = self.resolve_parent(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some((dst_ns, dst_scope, dst_parent_path)) = self.resolve_parent(newparent) else {
            reply.error(Errno::ENOENT);
            return;
        };

        if src_ns != dst_ns || src_scope != dst_scope {
            debug!("rename: cross-namespace/scope -> EXDEV");
            reply.error(Errno::EXDEV);
            return;
        }

        let src_ino = {
            let dc = self.dir_children.lock().unwrap();
            dc.get(&parent).and_then(|c| c.get(&name_str).copied())
        };
        let Some(src_ino) = src_ino else {
            reply.error(Errno::ENOENT);
            return;
        };

        let src_kind = {
            let inodes = self.inodes.lock().unwrap();
            inodes.get(&src_ino).map(|e| e.kind.clone())
        };
        let (src_path, is_file) = match src_kind {
            Some(InodeKind::RemoteFile { path, .. }) => (path, true),
            Some(InodeKind::RemoteDir { path, .. }) => (path, false),
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let dst_path = Self::make_child_path(&dst_parent_path, &newname_str);

        // Self-rename: short-circuit before any HTTP.
        if src_path == dst_path && parent == newparent {
            reply.ok();
            return;
        }

        if !is_file {
            debug!("rename: directory rename not supported natively -> EXDEV for copy+unlink fallback");
            reply.error(Errno::EXDEV);
            return;
        }

        // Single atomic call to the BE's rename endpoint.
        if let Err(e) = self.client.rename(&src_ns, &src_scope, &src_path, &dst_path) {
            error!("rename {src_ns}/{src_scope}: {src_path} → {dst_path} failed: {e}");
            reply.error(api_error_to_errno(&e));
            return;
        }

        // Backend state is consistent — update the inode tree atomically.
        {
            let mut inodes = self.inodes.lock().unwrap();
            let mut dc = self.dir_children.lock().unwrap();

            if let Some(children) = dc.get_mut(&parent) {
                children.remove(&name_str);
            }

            // Overwrite case: if dst already had an inode, drop it (the PUT
            // replaced its contents; the kernel will re-lookup and find the
            // moved source inode under the new name).
            let existing_dst = dc
                .get(&newparent)
                .and_then(|c| c.get(&newname_str).copied());
            if let Some(old_dst_ino) = existing_dst {
                if old_dst_ino != src_ino {
                    inodes.remove(&old_dst_ino);
                }
            }

            if let Some(entry) = inodes.get_mut(&src_ino) {
                entry.kind = InodeKind::RemoteFile {
                    namespace: src_ns.clone(),
                    scope: src_scope.clone(),
                    path: dst_path.clone(),
                };
                // Bust the cached body so a stale read of the renamed
                // inode (kernel hands us the moved ino) doesn't return
                // pre-rename bytes if a writer mutates dst out-of-band.
                entry.cache_time = None;
            }

            dc.entry(newparent).or_default().insert(newname_str, src_ino);
        }

        self.invalidate_cache(parent);
        if parent != newparent {
            self.invalidate_cache(newparent);
        }

        reply.ok();
    }
}
