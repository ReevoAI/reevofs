# AgentFS FUSE Layer (ReevoFS over Postgres + S3)

## Context

The Python backend now stores all AgentFS data in PostgreSQL (inline content + S3 for large files). The Rust FUSE layer (ReevoFS) needs to be rewritten to read/write against the same Postgres tables instead of local SQLite files.

## Current State

### Postgres Schema (source of truth)

```sql
-- File storage (path-first, no inodes)
CREATE TABLE agentfs_file (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope TEXT NOT NULL CHECK (scope IN ('system', 'org', 'user')),
    organization_id UUID,
    user_id UUID,
    path TEXT NOT NULL,
    is_directory BOOLEAN NOT NULL DEFAULT FALSE,
    size_bytes BIGINT NOT NULL DEFAULT 0,
    storage TEXT NOT NULL DEFAULT 'inline' CHECK (storage IN ('inline', 's3')),
    content BYTEA CHECK (octet_length(content) <= 262144),
    s3_key TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (scope, organization_id, user_id, path)
);

-- KV metadata store
CREATE TABLE agentfs_kv (
    scope TEXT NOT NULL CHECK (scope IN ('system', 'org', 'user')),
    organization_id UUID,
    user_id UUID,
    key TEXT NOT NULL,
    value JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (scope, organization_id, user_id, key)
);
```

### Python Layer (already working)

- `AgentFSRepository` — Postgres queries for file + KV CRUD
- `AgentFSFileService` — path-based file operations with namespace/scope validation
- `AgentFSSkillStore` — skill-specific CRUD (SKILL.md + KV index)
- `SkillScopeRegistry` — manages per-scope store instances
- `SkillService` — business logic with visibility hierarchy (user > org > system)

## FUSE Layer Design

### Architecture

```
FUSE mount (user process)
  └── WriteFilterFS        (unchanged — EACCES on protected paths)
       └── OverlayFS       (unchanged — whiteout/merge/copy-on-write)
            ├── user layer  → PgAgentFS { scope="user", org_id=X, user_id=Y }
            ├── org layer   → PgAgentFS { scope="org",  org_id=X }
            └── system      → PgAgentFS { scope="system" }
```

### PgAgentFS: New Rust FileSystem Implementation

Replace `AgentFS` (SQLite-backed) with `PgAgentFS` (Postgres-backed). Implements the same `FileSystem` trait.

```rust
struct PgAgentFS {
    pool: sqlx::PgPool,
    s3: Option<aws_sdk_s3::Client>,
    scope: String,              // "system" | "org" | "user"
    organization_id: Option<Uuid>,
    user_id: Option<Uuid>,

    // In-memory inode mapping (path ↔ inode)
    inode_map: DashMap<i64, String>,      // ino → path
    path_map: DashMap<String, i64>,       // path → ino
    next_ino: AtomicI64,
}
```

### Inode Strategy: Path-First with In-Memory Inode Cache

The Postgres schema is path-based (no inode table). The FUSE layer needs inodes because the kernel speaks in inode numbers. Strategy:

1. **On mount**: Load all paths for the scope from `agentfs_file`, assign sequential inode numbers
2. **On lookup**: Check `path_map`, assign new ino if path is new
3. **On create/mkdir**: Insert into Postgres, assign ino, update maps
4. **On unlink/rmdir**: Delete from Postgres, remove from maps

Inodes are ephemeral — they exist only while mounted. This is fine because:
- FUSE doesn't persist inodes across mounts
- The kernel cache is invalidated on unmount
- All durable state lives in Postgres

### FileSystem Trait Mapping

| FUSE Op | Postgres Query |
|---------|---------------|
| `lookup(parent_ino, name)` | `path_map[parent_path + "/" + name]` → if miss, `SELECT FROM agentfs_file WHERE scope=... AND path=...` |
| `getattr(ino)` | `inode_map[ino]` → path → `SELECT size_bytes, is_directory, created_at, updated_at FROM agentfs_file WHERE ...` |
| `readdir(ino)` | `inode_map[ino]` → parent_path → `SELECT path, is_directory FROM agentfs_file WHERE path LIKE parent_path/% AND path NOT LIKE parent_path/%/%` |
| `open(ino)` | Return file handle (ino stored for later read/write) |
| `read(fh, offset, size)` | `SELECT storage, content, s3_key FROM agentfs_file WHERE ...` — if S3, fetch from S3 |
| `write(fh, offset, data)` | Read current content, apply write at offset, `UPDATE agentfs_file SET content=..., size_bytes=...` |
| `create(parent_ino, name)` | `INSERT INTO agentfs_file` + assign ino |
| `mkdir(parent_ino, name)` | `INSERT INTO agentfs_file (is_directory=TRUE)` + assign ino |
| `unlink(parent_ino, name)` | `DELETE FROM agentfs_file WHERE ...` + remove from maps |
| `rmdir(parent_ino, name)` | Same as unlink (check empty first) |
| `rename(old_parent, old_name, new_parent, new_name)` | `UPDATE agentfs_file SET path=new_path WHERE path=old_path` + update descendants |
| `statfs()` | `SELECT count(*), sum(size_bytes) FROM agentfs_file WHERE scope=...` |

### OverlayFS Compatibility

The existing `OverlayFS` code works unchanged — it only depends on the `FileSystem` trait. It manages its own virtual inode space on top of whatever the underlying filesystem returns.

- Whiteouts stored in `fs_whiteout` table → move to `agentfs_file` with a `is_whiteout` flag, or keep as KV entries
- Copy-on-write → reads from base layer, writes to delta layer (separate scope params)
- Origin mapping → `agentfs_kv` entries for `origin:{delta_ino}` → `{base_ino}`

### KV Store

Same Postgres table, same queries. The Rust KV trait maps to:
- `kv.get(key)` → `SELECT value FROM agentfs_kv WHERE scope=... AND key=...`
- `kv.set(key, value)` → `INSERT ... ON CONFLICT DO UPDATE`
- `kv.delete(key)` → `DELETE FROM agentfs_kv WHERE ...`
- `kv.keys(prefix)` → `SELECT key FROM agentfs_kv WHERE key LIKE prefix%`

### Cache Invalidation

When Python writes to `agentfs_file`, the FUSE inode map becomes stale. Options:

1. **LISTEN/NOTIFY** (recommended): Python sends `NOTIFY agentfs_change, 'scope:org:uuid:path'` after writes. FUSE process subscribes and updates its map. Latency: ~1ms.
2. **Kernel cache TTL**: Set FUSE `entry_timeout` and `attr_timeout` to e.g. 1s. Kernel re-validates automatically.
3. **Poll**: FUSE periodically queries `updated_at > last_check`. Simple but laggy.

For skill files (rarely updated), option 2 (kernel TTL) is probably sufficient.

### S3 Integration

- Read: If `storage = 's3'`, fetch from S3 using `s3_key`
- Write: If content > 256KB, upload to S3, store key in Postgres
- Use `aws-sdk-s3` crate with the same bucket configured in settings

### Connection Management

```rust
impl PgAgentFS {
    async fn new(
        pool: sqlx::PgPool,
        s3: Option<aws_sdk_s3::Client>,
        scope: &str,
        organization_id: Option<Uuid>,
        user_id: Option<Uuid>,
    ) -> Result<Self> {
        let mut fs = Self {
            pool, s3, scope, organization_id, user_id,
            inode_map: DashMap::new(),
            path_map: DashMap::new(),
            next_ino: AtomicI64::new(2), // 1 = root
        };

        // Root inode
        fs.inode_map.insert(1, "/".into());
        fs.path_map.insert("/".into(), 1);

        // Pre-load all paths for this scope
        let rows = sqlx::query("SELECT path, is_directory FROM agentfs_file WHERE scope = $1 AND organization_id IS NOT DISTINCT FROM $2 AND user_id IS NOT DISTINCT FROM $3")
            .bind(&fs.scope)
            .bind(fs.organization_id)
            .bind(fs.user_id)
            .fetch_all(&fs.pool)
            .await?;

        for row in rows {
            let path: String = row.get("path");
            let ino = fs.next_ino.fetch_add(1, Ordering::Relaxed);
            fs.inode_map.insert(ino, path.clone());
            fs.path_map.insert(path, ino);
        }

        Ok(fs)
    }
}
```

### Stack Builder Update

```rust
// reevofs/src/stack.rs
pub async fn build_stack(pool: PgPool, s3: Option<S3Client>, layers: Vec<LayerConfig>) -> Result<WriteFilterFS> {
    // Bottom layer (system)
    let system = PgAgentFS::new(pool.clone(), s3.clone(), "system", None, None).await?;

    // Org layer overlay
    let org = PgAgentFS::new(pool.clone(), s3.clone(), "org", Some(org_id), None).await?;
    let org_overlay = OverlayFS::new(Arc::new(system), org);

    // User layer overlay
    let user = PgAgentFS::new(pool.clone(), s3.clone(), "user", Some(org_id), Some(user_id)).await?;
    let user_overlay = OverlayFS::new(Arc::new(org_overlay), user);

    // Wrap with write protection
    let protected = scan_protected_paths(&pool, &layers).await?;
    Ok(WriteFilterFS::new(user_overlay, protected))
}
```

### Mount Command

```bash
reevofs mount \
  --pg-url "postgresql://user:pass@host:5432/db" \
  --scope-system \
  --scope-org <org-uuid> \
  --scope-user <user-uuid> \
  --s3-bucket <bucket-name> \
  /mountpoint
```

## Migration Path

1. **Phase 1** (done): Python backend uses Postgres — no SQLite/Turso
2. **Phase 2** (this spec): Implement `PgAgentFS` in Rust, plug into existing OverlayFS/WriteFilterFS
3. **Phase 3** (optional): Add `LISTEN/NOTIFY` for real-time cache invalidation between Python and FUSE

## Key Decisions

- **No inode table in Postgres** — inodes are ephemeral FUSE-side caches, rebuilt on mount
- **Same tables for Python and Rust** — single source of truth
- **OverlayFS unchanged** — only the leaf `FileSystem` implementation changes
- **`IS NOT DISTINCT FROM`** for NULL-safe scope matching (system scope has NULL org/user)
