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
    scope TEXT NOT NULL CHECK (scope IN ('system', 'organization', 'user')),
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
    scope TEXT NOT NULL CHECK (scope IN ('system', 'organization', 'user')),
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

The agent sees a **merged view** — skills from all scopes (system, organization, user) are flattened into a single directory. Higher-priority scopes shadow lower ones (user > organization > system).

```
/skills/                          ← ReevoFS mount point (merged view)
├── email-drafting/
│   └── SKILL.md                  ← from system scope (or org override)
├── custom-crm/
│   └── SKILL.md                  ← from organization scope
├── my-personal-skill/
│   └── SKILL.md                  ← from user scope
└── ...
```

The agent does NOT see `system/`, `organization/`, `user/` subdirectories. Scope resolution happens inside ReevoFS — it queries all three scopes via the API and merges results with deduplication (user > org > system priority).

For writes, ReevoFS writes to the user scope by default. System and org skills are read-only from the agent's perspective.

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

For reads, ReevoFS queries all three scopes and merges (user > org > system). For writes, defaults to user scope.

| FUSE Op | API Call | Scope Behavior |
|---------|----------|---------------|
| `lookup(parent, name)` | Populate via `POST /_list` on each scope | Merged: first match wins (user > org > system) |
| `getattr(ino)` | Return cached attrs | From whichever scope owns the file |
| `readdir(ino)` | `POST /_list` on all 3 scopes, deduplicate | Merged listing, user shadows org shadows system |
| `read(ino, offset, size)` | `GET /{ns}/{scope}/{path}` | Read from owning scope |
| `write(ino, offset, data)` | Buffer in memory | — |
| `flush(ino, fh)` | `PUT /{ns}/user/{path}` | Always writes to user scope |
| `create(parent, name)` | `PUT /{ns}/user/{path}` | Creates in user scope |
| `mkdir(parent, name)` | `PUT /{ns}/user/{path}/.keep` | Creates in user scope |
| `unlink(parent, name)` | `DELETE /{ns}/user/{path}` | Only deletes from user scope |
| `rmdir(parent, name)` | `DELETE /{ns}/user/{path}` | Only deletes from user scope |

System and org skills appear in listings but return `EACCES` on write/delete attempts.

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

## AgentCore Sandbox Integration

### Ask Reevo Execution Flow

Ask Reevo uses sandbox containers with AgentCore in us-west to execute bash commands. AgentCore itself has no knowledge of ReevoFS — we mount it during our container bootup process before the agent starts.

```
User → Ask Reevo → Backend provisions sandbox container
                      ├── Container bootup script:
                      │     1. Inject auth token into env (outside agent control)
                      │     2. Mount ReevoFS: reevofs mount /skills
                      │     3. Start AgentCore
                      ├── Container ID is reused across requests
                      └── Agent reads skills from /skills/ as regular files
```

### Token Flow

1. Backend creates/reuses a sandbox container with a stable container ID
2. Backend appends an auth token (JWT with `user_id`, `org_id`) to the container environment — **outside agent control**, the agent never sees or manages tokens
3. Container bootup script runs `reevofs mount /skills` which reads the token from env
4. The agent sees skills as regular files — `cat /skills/email-drafting/SKILL.md` just works

### Container Lifecycle

```
Container (persistent per session):
  ├── First request: container created, token injected, reevofs mounted, AgentCore started
  ├── Subsequent requests: same container ID reused, mount persists
  └── Session end: container stopped (or recycled)
```

### Environments

| Environment | API Target | Container Runtime |
|-------------|-----------|-------------------|
| Production | `api.reevo.ai` | AgentCore containers in us-west |
| Development | `api-dev.reevo.ai` | AgentCore dev containers |
| Local dev | `localhost:8000` | AgentCore dev (same containers, local API) |

Local dev runs against AgentCore dev containers, with the API pointing to localhost.

## Implementation Status

- [x] Phase 1: Python backend with Postgres (done — PR merged)
- [x] Phase 2: Rust FUSE layer via REST API (done — current implementation)
- [ ] Phase 3: AgentCore sandbox integration (token injection + mount)
- [ ] Phase 4: Add `outputs` namespace for agent output mounts
- [ ] Phase 5: HTTP/2 + connection pooling optimization
- [ ] Phase 6: WebSocket or SSE for push-based cache invalidation (optional)

## Key Decisions

- **No direct SQL** — API is the only data path, enforcing auth and scope at the boundary
- **No OverlayFS** — scopes are separate directories, not layered; the API handles visibility
- **No DB/S3 credentials in sandbox** — only API token needed
- **Token appended externally** — agent never sees or controls its own auth
- **Ephemeral inodes** — rebuilt per mount, no persistence needed
- **Write buffering** — reduces API calls, full content sent on flush
- **Container reuse** — same container ID across requests, mount persists
