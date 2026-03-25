# AgentFS FUSE Layer (ReevoFS over Reevo API)

## Context

The Python backend stores all AgentFS data in PostgreSQL (inline content + S3 for large files). The Rust FUSE layer (ReevoFS) mounts this as a local filesystem by calling the backend's REST API — no direct database access.

## Design Principles

- **No SQL in the sandbox** — agents never get database credentials
- **API is the security boundary** — auth, scope validation, path traversal checks all happen server-side
- **HTTP/2 for performance** — multiplexed requests over a single connection (behind ALB/TLS in production)
- **Ephemeral inodes** — assigned on demand, cached in memory, no persistence needed

## Backend (source of truth)

### Postgres Schema

```sql
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

### Python Services (already working)

- `AgentFSRepository` — Postgres queries for file + KV CRUD
- `AgentFSFileService` — path-based file operations with namespace/scope validation
- `AgentFSSkillStore` — skill-specific CRUD (SKILL.md + KV index)
- `SkillScopeRegistry` — manages per-scope store instances
- `SkillService` — business logic with visibility hierarchy (user > org > system)

### REST API (`/api/v2/fs/`)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/{namespace}/{scope}/{path}` | Read file content |
| PUT | `/{namespace}/{scope}/{path}` | Write (create/overwrite) file |
| DELETE | `/{namespace}/{scope}/{path}` | Delete file or directory (recursive) |
| POST | `/{namespace}/{scope}/_list` | List directory contents |

Auth: `x-reevo-user-id` + `x-reevo-org-id` headers, or `Authorization: Bearer <jwt>`.

Scope permissions:
- `system` — read-only via API
- `org` — admin-only writes
- `user` — owner-only writes

## FUSE Layer Design

### Architecture

```
FUSE mount (agent sandbox)
  └── ReevoFS (Rust)
       ├── In-memory inode cache (ephemeral, 5s TTL)
       ├── Write buffer (per file handle, flush on close)
       └── HTTP/2 client → Reevo API
                              └── Postgres + S3
```

No direct database access. No OverlayFS. No WriteFilterFS. Scope isolation and write protection are enforced by the API layer.

### Mount Layout

```
/mountpoint/
├── skills/
│   ├── system/   → GET/POST /api/v2/fs/skills/system/...
│   ├── org/      → GET/PUT/DELETE/POST /api/v2/fs/skills/org/...
│   └── user/     → GET/PUT/DELETE/POST /api/v2/fs/skills/user/...
└── outputs/      (future namespace)
    ├── org/
    └── user/
```

### ReevoFS Implementation

```rust
struct ReevoFS {
    client: ReevoClient,          // HTTP client → Reevo API
    inodes: Mutex<HashMap<u64, InodeEntry>>,
    dir_children: Mutex<HashMap<u64, HashMap<String, u64>>>,
    next_ino: AtomicU64,
    write_buffers: Mutex<HashMap<u64, WriteBuffer>>,
    next_fh: AtomicU64,
    namespaces: Vec<(String, Vec<String>)>,
}
```

### Inode Strategy

Inodes are ephemeral — allocated on demand, never persisted:

1. **On mount**: Virtual tree built for `/{namespace}/{scope}` directories
2. **On readdir/lookup**: API call to list/check, new inodes assigned for discovered entries
3. **On create/mkdir**: API call to write, new inode assigned locally
4. **On unlink/rmdir**: API call to delete, inode removed from cache
5. **Cache TTL**: 5 seconds — balances freshness with API call volume

### FUSE Op → API Mapping

| FUSE Op | API Call |
|---------|----------|
| `lookup(parent, name)` | Populate via `POST /_list`, check cache |
| `getattr(ino)` | Return cached attrs (size from content cache) |
| `readdir(ino)` | `POST /{ns}/{scope}/_list` with parent path |
| `read(ino, offset, size)` | `GET /{ns}/{scope}/{path}`, cache content |
| `write(ino, offset, data)` | Buffer in memory |
| `flush(ino, fh)` | `PUT /{ns}/{scope}/{path}` with buffered content |
| `create(parent, name)` | `PUT /{ns}/{scope}/{path}` with empty content |
| `mkdir(parent, name)` | `PUT /{ns}/{scope}/{path}/.keep` (creates parent) |
| `unlink(parent, name)` | `DELETE /{ns}/{scope}/{path}` |
| `rmdir(parent, name)` | `DELETE /{ns}/{scope}/{path}` (recursive) |

### Write Path

Writes are buffered in memory per file handle and flushed to the API on close/flush:

1. `create()` → API `PUT` with empty content, allocate write buffer
2. `write()` → append to in-memory buffer (no API call)
3. `flush()`/`release()` → API `PUT` with full buffer content
4. Update local inode cache with new content

### Cache Invalidation

- **Kernel TTL**: FUSE `entry_timeout` and `attr_timeout` set to 5s
- **Content cache**: 5s TTL on file content, re-fetched on next read after expiry
- **Directory cache**: 5s TTL on listings, re-populated from API after expiry
- **Write-through**: Local cache updated immediately after successful API writes

For agent workloads (short-lived sessions, single writer), this is sufficient. If multi-writer scenarios arise, the API could add `ETag`/`If-Match` headers for optimistic concurrency.

### Security Model

```
Agent sandbox
  ├── Has: API URL + auth token (user_id, org_id)
  ├── Has: FUSE mount at /workspace or similar
  └── Does NOT have: DB credentials, S3 credentials, network access to DB

Reevo API (trusted boundary)
  ├── Validates auth headers on every request
  ├── Enforces scope permissions (system=RO, org=admin, user=owner)
  ├── Rejects path traversal (.. in paths)
  ├── Handles storage tiering (inline ≤256KB, S3 for larger)
  └── Manages Postgres + S3 connections
```

### Mount Command

```bash
reevofs mount \
  --api-url https://api.reevo.ai \
  --user-id <user-uuid> \
  --org-id <org-uuid> \
  --token <jwt-or-api-key> \
  /mountpoint
```

Or via environment variables:
```bash
export REEVO_API_URL=https://api.reevo.ai
export REEVO_USER_ID=<user-uuid>
export REEVO_ORG_ID=<org-uuid>
export REEVO_API_TOKEN=<token>
reevofs mount /mountpoint
```

### CLI (no FUSE required)

For environments without FUSE (macOS without macFUSE, minimal containers):

```bash
reevofs ls /                        # list root
reevofs ls -s user /                # list user scope
reevofs cat /email-drafting/SKILL.md
reevofs write -c "content" /path
echo "content" | reevofs write /path
```

## Implementation Status

- [x] Phase 1: Python backend with Postgres (done)
- [x] Phase 2: Rust FUSE layer via REST API (done — current implementation)
- [ ] Phase 3: Add `outputs` namespace for agent output mounts
- [ ] Phase 4: HTTP/2 + connection pooling optimization
- [ ] Phase 5: WebSocket or SSE for push-based cache invalidation (optional)

## Key Decisions

- **No direct SQL** — API is the only data path, enforcing auth and scope at the boundary
- **No OverlayFS** — scopes are separate directories, not layered; the API handles visibility
- **No DB/S3 credentials in sandbox** — only API token needed
- **Ephemeral inodes** — rebuilt per mount, no persistence needed
- **Write buffering** — reduces API calls, full content sent on flush
