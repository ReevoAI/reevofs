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
up to the existing 100 MiB cap (`agentfs_max_write_bytes`), with the
server transparently routing between inline storage (≤256 KiB) and S3
(>256 KiB) — the same tiering the read path already exposes, but
invisible to the client. Same 415 blocklist as reads.

## Current state

```
PUT /api/v2/fs/{namespace}/{scope}/{path}
Content-Type: application/json

{"content": "<utf-8 string>"}

→ 200 {"success": true, "path": "/..."}
```

- Body is forced through `str`, so binary bytes are lossy.
- Extension blocklist (`.exe/.sh/.bat/.bin/.dll/.so/.dylib`) is only
  enforced on read — executables can be written today.
- The 100 MiB body cap and inline-vs-S3 tiering already exist
  server-side (`settings.agentfs_max_write_bytes`,
  `settings.agentfs_inline_threshold_bytes`); this spec does not change
  either.

## Proposed contract

One endpoint, raw bytes, up to 100 MiB. The server decides inline vs S3
based on body size — the client never sees the tier boundary.

```
PUT /api/v2/fs/{namespace}/{scope}/{path}
Content-Type: application/octet-stream
Content-Length: <= 104857600

<raw bytes>

→ 201 Created
  ETag: "<row_id>-<updated_at_unix_ms>"
  Location: /api/v2/fs/{namespace}/{scope}/{path}
  Content-Type: application/json

  {"path": "/...", "size_bytes": 1234, "storage": "inline" | "s3"}
```

- Body is the file's bytes, verbatim, no wrapper.
- Hard cap: `agentfs_max_write_bytes` (100 MiB today). Over that → 413.
- Server routes storage:
  - ≤ `agentfs_inline_threshold_bytes` (256 KiB): write to
    `agentfs_file.content BYTEA`.
  - Above: stream the request body straight to S3 under
    `agentfs/<org>/<uuid>`, then insert the row with
    `storage = s3` + `s3_key` in the same transaction.
- Server sniffs MIME from extension + magic bytes (reuses
  `resolve_content_type` from the read path) and records it on the row
  so subsequent reads can serve `Content-Type` without re-sniffing.
- `storage` is returned in the response for observability only; the
  client is not expected to branch on it.
- `ETag` header lets clients do optimistic concurrency on subsequent
  writes via `If-Match`.

### Streaming semantics

The server reads the request body as a stream and writes it to S3 in
chunks when the running total exceeds the inline threshold. Under the
hood this means `multipart_upload` or an equivalent — no buffering the
full 100 MiB in memory. If the stream closes before `Content-Length`
bytes arrive, the partial S3 object is aborted and no row is inserted.

This is the "server-side proxy" alternative called out as v1 in the
earlier draft of this spec; we're committing to it as the canonical
path. Client-direct-to-S3 (presigned PUT) is deferred — see
"Deferred: >100 MiB" below.

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

### Errors

| Status | Meaning |
|--------|---------|
| 400    | Bad path, bad scope |
| 403    | Scope forbidden for caller (auth/sandbox) |
| 404    | Parent namespace doesn't exist |
| 412    | `If-Match` / `If-None-Match` precondition failed |
| 413    | Body exceeds `agentfs_max_write_bytes` (100 MiB) |
| 415    | Blocked extension |

## Backend changes

Modeled on PR #22934's surface area. Paths relative to `salestech-be/`.

### `salestech_be/web/api/fs/views.py`
- `write_file` (existing `@router.put`): drop `WriteFileRequest`,
  read body as `bytes` via `await request.body()` (or stream via
  `request.stream()` when over the inline threshold), enforce size cap,
  return `201` with `ETag` header and the new metadata JSON.

### `salestech_be/web/api/fs/schema.py`
- Delete `WriteFileRequest` (was `content: str`).
- Add `WriteFileResponse` (with `path`, `size_bytes`, `storage`,
  `etag`).

### `salestech_be/core/agentfs/file_service.py`
- `AgentFSFileService.write_file(..., content: bytes | AsyncIterator[bytes])`
  — signature change, routes inline vs S3 on size.
- Uses the existing `agentfs_inline_threshold_bytes` to decide tier.

### `salestech_be/db/dao/agentfs_repository.py`
- `write_inline_bytes_by_row_id(row_id, content: bytes)` — was `str`.
- `create_file_row(..., storage: AgentFSStorageType, s3_key: str | None)`.
- `lookup_row_by_path_for_write(...)` — atomic upsert helper.

### `salestech_be/web/api/fs/mime.py`
- Reuse the same `resolve_content_type` used on the read path; no
  change needed, just imported from the write path.

### Tests (`tests/integration/web/api/fs/`)
- `test_binary_write.py` — symmetric to `test_binary_read.py`:
  - inline round-trip with `\xff\xfe\xfd\xfc`
  - 256 KiB boundary (inline) and 256 KiB + 1 byte (S3 tier)
  - 100 MiB cap boundary (100 MiB + 1 byte → 413)
  - blocked extension (415)
  - `If-None-Match: *` create-only semantics
  - `If-Match` version check
- `test_binary_write_s3.py` — S3-tier write: full body streams through,
  row gets `storage=s3`, subsequent GET follows the 302 to S3.

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

The shim already handles files up to 100 MiB transparently — it PUTs
whatever bytes it has, and the server decides where they land. No
follow-up work needed for the inline/S3 boundary.

### webapp

If the webapp uses `saveArtifact` / any POST-back for agent output,
it will need the same migration. Out of scope for this spec — the
webapp's current Artifacts path is read-only.

## Rollout

1. **Backend PR lands with both contracts supported for ≤1 release.**
   PUT accepts both `application/json` (legacy) and
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

## Deferred: >100 MiB

For payloads over `agentfs_max_write_bytes` (100 MiB today) we would
need either a raised cap or a client-direct-to-S3 presigned-PUT
contract (two-phase: `:initiate-upload` → presigned S3 PUT →
`:finalize-upload`). This is deferred. Current agent workloads
(screenshots, PDFs, matplotlib output, small CSVs) sit comfortably
under 100 MiB; raise the cap or add the two-phase path only if
real workloads start hitting the limit.

## Open questions

1. **Does the writable namespace list change?** Today only `output`
   (and `skills/user`) are writable. Raw-bytes write shouldn't change
   the ACL model — same scopes, same auth checks, just a different
   body format.

2. **Content-Type on write:** should the client supply it, or should
   the server always sniff? The read path sniffs. For symmetry, sniff
   on write and ignore whatever the client sends, but record the
   sniffed type on the row so read can serve it without re-sniffing.
