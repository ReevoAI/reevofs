//! FUSE filesystem backed by the Reevo API.
//!
//! Mount layout:
//!   /{namespace}/{scope}/...files...
//!
//! Currently: /skills/system/..., /skills/org/..., /skills/user/...
//!
//! Requires the `fuse` feature and macFUSE (macOS) or libfuse (Linux).

#![cfg(feature = "fuse")]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyWrite, Request, WriteFlags,
};
use log::{debug, error, warn};

use reevofs_api::{ApiError, ReevoClient};

const TTL: Duration = Duration::from_secs(5);
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

impl ReevoFS {
    pub fn new(client: ReevoClient) -> Self {
        let namespaces = vec![(
            "skills".to_string(),
            vec!["system".to_string(), "org".to_string(), "user".to_string()],
        )];

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
            let ns_entry = InodeEntry {
                ino: ns_ino,
                kind: InodeKind::Namespace { name: ns_name.clone() },
                children: Vec::new(),
                content: None,
                cache_time: None,
            };
            self.inodes.get_mut().unwrap().insert(ns_ino, ns_entry);
            self.dir_children.get_mut().unwrap().insert(ns_ino, HashMap::new());

            self.inodes.get_mut().unwrap().get_mut(&ROOT_INO).unwrap().children.push(ns_ino);
            self.dir_children.get_mut().unwrap().get_mut(&ROOT_INO).unwrap().insert(ns_name.clone(), ns_ino);

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
            Ok(resp) => {
                let data = resp.content.into_bytes();
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

        let dir_children = self.dir_children.lock().unwrap();
        if let Some(children) = dir_children.get(&parent) {
            if let Some(&child_ino) = children.get(&name_str) {
                let inodes = self.inodes.lock().unwrap();
                if let Some(entry) = inodes.get(&child_ino) {
                    let attr = match &entry.kind {
                        InodeKind::RemoteFile { .. } => {
                            let size = entry.content.as_ref().map(|c| c.len() as u64).unwrap_or(0);
                            Self::file_attr(child_ino, size)
                        }
                        _ => Self::dir_attr(child_ino),
                    };
                    reply.entry(&TTL, &attr, Generation(0));
                    return;
                }
            }
        }

        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino: u64 = ino.into();
        let inodes = self.inodes.lock().unwrap();
        if let Some(entry) = inodes.get(&ino) {
            let attr = match &entry.kind {
                InodeKind::RemoteFile { .. } => {
                    let size = entry.content.as_ref().map(|c| c.len() as u64).unwrap_or(0);
                    Self::file_attr(ino, size)
                }
                _ => Self::dir_attr(ino),
            };
            reply.attr(&TTL, &attr);
        } else {
            reply.error(Errno::ENOENT);
        }
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

        match self.client.write_file(&namespace, &scope, &file_path, "") {
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
                reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
            }
            Err(e) => {
                error!("create failed: {e}");
                reply.error(Errno::EIO);
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
            let content = String::from_utf8_lossy(&buf.data).to_string();
            let ns = buf.namespace.clone();
            let scope = buf.scope.clone();
            let path = buf.path.clone();
            drop(buffers);

            match self.client.write_file(&ns, &scope, &path, &content) {
                Ok(_) => {
                    let data = content.into_bytes();
                    let mut inodes = self.inodes.lock().unwrap();
                    if let Some(e) = inodes.get_mut(&ino) {
                        e.content = Some(data);
                        e.cache_time = Some(SystemTime::now());
                    }
                    reply.ok();
                }
                Err(e) => {
                    error!("flush write_file failed: {e}");
                    reply.error(Errno::EIO);
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

        match self.client.write_file(&namespace, &scope, &placeholder_path, "") {
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
                reply.error(Errno::EIO);
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
                            reply.error(Errno::EIO);
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
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                }
            }
        }
        reply.error(Errno::ENOENT);
    }
}
