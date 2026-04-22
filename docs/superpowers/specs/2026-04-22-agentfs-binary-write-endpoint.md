# AgentFS binary write endpoint

**Status:** proposed
**Companion to:** [2026-04-20 agentfs binary read endpoint design](./2026-04-20-agentfs-binary-read-endpoint-design.md) (salestech-be PR #22934)
**Shim side:** reevofs `feat/raw-bytes-read-contract` branch (v0.3.8)

## Context

PR #22934 migrated `GET /api/v2/fs/{namespace}/{scope}/{path}` from a
`{path, content: string}` JSON envelope to raw bytes so binary files
(PNG, PDF, spreadsheets) round-trip byte-for-byte. The write endpoint
was intentionally left out of that PR — its scope was "unblock the
webapp Artifacts gallery preview," which is read-only.

That leaves the PUT endpoint as the last UTF-8 chokepoint in the
AgentFS data path. Any agent action that produces binary output
(`cp foo.png /reevofs/output/`, `matplotlib.savefig` into the mount,
screenshot upload, generated PDF) silently corrupts the first non-UTF-8
byte because the shim must `String::from_utf8_lossy` the bytes to fit
them into `WriteFileRequest.content: str`.

This spec proposes the symmetric write contract: raw-bytes PUT bodies
with the same tiered (inline ≤256 KiB / S3 >256 KiB) storage policy
and the same 415 blocklist as reads.

## Current state

```
PUT /api/v2/fs/{namespace}/{scope}/{path}
Content-Type: application/json

{"content": "<utf-8 string>"}

→ 200 {"success": true, "path": "/..."}
```

- Body is forced through `str`, so binary bytes are lossy.
- All writes hit the app server even for multi-megabyte payloads.
- No Content-Type on the payload → the service can't sniff or
  cache intelligently.
- Extension blocklist (`.exe/.sh/.bat/.bin/.dll/.so/.dylib`) is only
  enforced on read — executables can be written today.

## Proposed contract

### PUT — small/medium files (inline tier)

```
PUT /api/v2/fs/{namespace}/{scope}/{path}
Content-Type: application/octet-stream
Content-Length: <= 262144

<raw bytes>

→ 201 Created
  ETag: "<row_id>-<updated_at_unix_ms>"
  Location: /api/v2/fs/{namespace}/{scope}/{path}
  Content-Type: application/json

  {"path": "/...", "size_bytes": 1234, "storage": "inline"}
```

- Body is the file's bytes, verbatim, no wrapper.
- Size cap matches `agentfs_file.content BYTEA CHECK (octet_length <= 262144)`.
- Server sniffs MIME from extension + magic bytes for future
  `Content-Type` on read (same `resolve_content_type` used by the read
  path — no new logic).
- Response is JSON metadata. `ETag` header lets clients do optimistic
  concurrency on subsequent writes via `If-Match`.

### PUT — large files (S3 tier, two-phase)

Uploading a 50 MB CSV through the app server is wasteful. Match the
read path's 302-to-presigned pattern, but inverted:

**Phase 1 — initiate:**

```
POST /api/v2/fs/{namespace}/{scope}/{path}:initiate-upload
Content-Type: application/json

{"size_bytes": 52428800, "content_type": "text/csv"}

→ 200
  {
    "upload_url": "https://s3.../?X-Amz-Signature=...",
    "upload_method": "PUT",
    "upload_headers": {"Content-Type": "text/csv"},
    "finalize_url": "/api/v2/fs/{namespace}/{scope}/{path}:finalize-upload",
    "expires_in": 300,
    "s3_key": "agentfs/<org>/<uuid>"
  }
```

**Phase 2 — client PUTs directly to S3**, then:

**Phase 3 — finalize:**

```
POST /api/v2/fs/{namespace}/{scope}/{path}:finalize-upload
Content-Type: application/json

{"s3_key": "agentfs/<org>/<uuid>", "size_bytes": 52428800}

→ 201 Created
  ETag: "<row_id>-<updated_at_unix_ms>"
  {"path": "/...", "size_bytes": 52428800, "storage": "s3"}
```

The finalize step verifies the object exists in S3, reads its `size_bytes`
and `Content-Type`, then creates the `agentfs_file` row transactionally.
If finalize never arrives, S3 lifecycle policy reaps the orphaned key
after N days — matches the read path's presigned-URL expiry model.

**Alternative simpler path for v1 — server-side proxy:** accept
`Transfer-Encoding: chunked` on the inline PUT endpoint, stream
straight through to S3 if size exceeds the inline cap mid-stream,
and write `storage = s3` + `s3_key` on commit. Cheaper to implement
(no new endpoints), more expensive in bytes-through-app-server. See
"Open questions" below.

### Blocklist (415)

```
PUT .../payload.exe
Content-Type: application/octet-stream
<bytes>

→ 415 Unsupported Media Type
  Content-Type: application/problem+json
  {"type":"about:blank","title":"Unsupported Media Type",
   "status":415,"extension":"exe"}
```

Reuse `resolve_content_type` / `UnsupportedMediaTypeError` from the read
path. Blocklist applies identically on read and write — agents can't
upload executables and then read them back elsewhere.

### Preconditions

- `If-Match: <etag>` → `412 Precondition Failed` when the ETag doesn't
  match the current row. Optional in v1; required once the agent
  runtime does parallel writes.
- `If-None-Match: *` → `412` if the file already exists. Useful for
  create-only semantics.

### Errors (unchanged from JSON contract)

| Status | Meaning |
|--------|---------|
| 400    | Bad path, bad scope, size over cap with no `:initiate-upload` |
| 403    | Scope forbidden for caller (auth/sandbox) |
| 404    | Parent namespace doesn't exist |
| 412    | `If-Match` / `If-None-Match` precondition failed |
| 413    | Body exceeds inline cap (directs client to `:initiate-upload`) |
| 415    | Blocked extension |

## Backend changes

Modeled on PR #22934's surface area. Paths relative to `salestech-be/`.

### `salestech_be/web/api/fs/views.py`
- `write_file` (existing `@router.put`): drop `WriteFileRequest`,
  read body as `bytes` via `await request.body()`, enforce size cap,
  return `201` with `ETag` header and the new metadata JSON.
- Add `initiate_upload` (POST `:initiate-upload`) — returns presigned
  S3 PUT URL, validates `size_bytes` against a hard cap (e.g. 100 MiB),
  runs blocklist, does not create a DB row.
- Add `finalize_upload` (POST `:finalize-upload`) — HEADs the S3 key,
  inserts the `agentfs_file` row in a transaction.

### `salestech_be/web/api/fs/schema.py`
- Delete `WriteFileRequest` (was `content: str`).
- Add `InitiateUploadRequest`, `InitiateUploadResponse`,
  `FinalizeUploadRequest`, `WriteFileResponse` (with `size_bytes`,
  `storage`, `etag`).

### `salestech_be/core/agentfs/file_service.py`
- `AgentFSFileService.write_file(..., content: bytes)` — signature
  change, route inline vs S3 on size.
- New: `generate_write_presigned_url(s3_key, content_type, expires_in)`
  mirroring `generate_read_presigned_url`.
- New: `finalize_write(s3_key, size_bytes, ...)` — HEADs S3, inserts row.

### `salestech_be/db/dao/agentfs_repository.py`
- `write_inline_bytes_by_row_id(row_id, content: bytes)` — was `str`.
- `create_file_row(..., storage: AgentFSStorageType, s3_key: str | None)`.
- `lookup_row_by_path_for_write(...)` — atomic upsert helper used by
  both inline PUT and finalize.

### `salestech_be/web/api/fs/mime.py`
- Export the same `resolve_content_type` used on the read path; no
  change needed, just imported from the write path.

### Tests (`tests/integration/web/api/fs/`)
- `test_binary_write.py` — symmetric to `test_binary_read.py`:
  - inline round-trip with `\xff\xfe\xfd\xfc`
  - 256 KiB boundary (inline) and 256 KiB + 1 byte (413 → initiate)
  - blocked extension (415)
  - `If-None-Match: *` create-only semantics
  - `If-Match` version check
- `test_upload_lifecycle.py` — initiate + simulated S3 PUT + finalize
  round-trip; orphan finalize (no S3 object → 400).

## Caller migration

### reevofs shim (this branch, v0.3.8)

Already done:

- `reevofs_api::ReevoClient::write_file(..., content: &[u8])` — sends
  raw body with `Content-Type: application/octet-stream`
  (`crates/api/src/lib.rs`).
- `flush_write_fd` / `flush_write_fd_no_invalidate` — pass memfd bytes
  through unmodified (`crates/preload/src/lib.rs`).
- `try_rename_reevofs` — raw bytes on the PUT
  (`crates/preload/src/lib.rs`).
- FUSE CLI `flush` — raw bytes (`crates/cli/src/fs.rs`).
- Mock API (`tests/mock_api.py`) stores raw bytes on PUT and returns
  415 for blocked extensions.
- Integration tests (`tests/integration_test.sh`) assert byte-exact
  MD5 on both `mv` and `cp` of the all-256-byte-values binary fixture.
- Unit tests (`crates/api/tests/write_raw_bytes.rs`) assert octet-stream
  header on PUT and 415 → `Forbidden`.

Outstanding (gated on backend): inline > 256 KiB currently no-ops
at the mock level — the shim does not yet know to fall back to
`:initiate-upload`. Track in a v0.4 follow-up once the large-file
lifecycle endpoints exist.

### webapp

If the webapp uses `saveArtifact` / any POST-back for agent output,
it will need the same migration. Out of scope for this spec — the
webapp's current Artifacts path is read-only.

## Rollout

1. **Backend PR lands with both contracts supported for ≤1 release.**
   Inline PUT accepts both `application/json` (legacy) and
   `application/octet-stream` (new) behind a feature flag
   `AGENTFS_BINARY_WRITE_ENABLED`. Response shape stays
   `{success, path}` when called via JSON, adds `size_bytes/storage/etag`
   when called via octet-stream.
2. **Shim v0.3.8 rolls out behind a config toggle** — the preload
   checks `REEVOFS_WRITE_CONTRACT=bytes|json` (default `json` for one
   release, flip to `bytes` once the backend flag is on in prod).
3. **Flip the backend feature flag.** Shim toggle flips to `bytes`
   via the sandbox image rebuild.
4. **Remove JSON write path** one release later. Delete
   `WriteFileRequest`, delete the toggle from the shim.

Alternative: cut over in one shot without toggles. The read PR did
this (it's a hard migration), and it worked because the read surface
is smaller. Writes have more call sites (`flush_write_fd`,
`flush_write_fd_no_invalidate`, `try_rename_reevofs`, FUSE `flush`)
and fail more silently (mojibake, not a crash). Recommend the toggle
for writes.

## Open questions

1. **Two-phase presigned PUT vs. server-side streaming for large
   files?** Two-phase matches the read contract's presigned-URL
   pattern and keeps large payloads off the app server; server-side
   streaming is simpler (one endpoint) and avoids a client-side S3
   dependency but costs app server bandwidth.
   **Recommendation:** ship two-phase. The read path already has the
   client-side S3 machinery (ureq follows the 302 via
   `redirects(3)`); reusing it costs little.

2. **Does the writable namespace list change?** Today only `output`
   (and `skills/user`) are writable. Raw-bytes write shouldn't change
   the ACL model — same scopes, same auth checks, just a different
   body format.

3. **Content-Type on write:** should the client supply it, or should
   the server always sniff? The read path sniffs. For symmetry, sniff
   on write and ignore whatever the client sends, but record the
   sniffed type on the row so read can serve it without re-sniffing.

4. **Chunked upload** for very large files (>100 MiB)? Not needed for
   the current agent workload (screenshots, small PDFs); revisit if
   users start generating multi-GB CSVs.
