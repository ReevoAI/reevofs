# AgentFS native rename endpoint

**Status:** proposed (ask for salestech-be)
**Author:** reevofs / FUSE edit-parity work
**Date:** 2026-05-14
**Companion to:** [2026-04-22 agentfs binary write endpoint](./2026-04-22-agentfs-binary-write-endpoint.md)

## TL;DR

We need `POST /api/v2/fs/{namespace}/{scope}/{path}?op=rename` (or any
equivalent server-side move verb). Today the FUSE layer must emulate
rename with **GET + PUT + DELETE**, which transfers the file's bytes
twice, has a non-atomic window where both paths exist, and creates a
new row instead of preserving the original's `created_at`. A single
backend call removes all three problems.

## Why this matters

`rename(2)` is the operation that unblocks editor saves. Every common
"safe write" library — Python's `open(p, 'w')` via NamedTemporaryFile,
sed -i, vim/nano, npm/poetry lockfile writers, atomic JSON writers —
writes to a tempfile first and then renames over the destination. The
LD_PRELOAD shim handles this transparently because POSIX rename on the
host filesystem already works; FUSE has to implement the verb itself.

The current FUSE workaround:

```
1. GET  /api/v2/fs/{ns}/{scope}/{src}   → N bytes
2. PUT  /api/v2/fs/{ns}/{scope}/{dst}   ← N bytes
3. DELETE /api/v2/fs/{ns}/{scope}/{src}
```

Three round-trips, 2N bytes over the wire for an operation that's
metadata-only at the storage layer.

## Concrete problems with the GET+PUT+DELETE workaround

1. **Non-atomic.** Between step 2 and step 3 both paths exist. If the
   client crashes, the source remains and an agent re-running the same
   tool sees two copies. If step 3 fails after step 2 succeeded, the
   FUSE layer attempts a rollback DELETE on the destination — but if
   *that* also fails (network blip, sandbox JWT expiry mid-op), the
   filesystem is left with both copies and no automatic recovery.
2. **Loses identity.** A native rename keeps the same row /
   `created_at` / lineage. PUT-to-new-path creates a fresh row. Any
   downstream system that joins on file identity (audit log, citation
   tracking, chat-attachment ↔ message linkage) sees a "delete + new
   file" rather than a move.
3. **Doubles the byte transfer.** A 50 MB chat attachment rename
   currently downloads 50 MB and re-uploads 50 MB through the agent
   sandbox's egress. With S3-backed >256 KB storage, a native rename
   is one S3 `CopyObject` (server-side, free, fast) or — if we
   structure it as a row-level path update — zero S3 traffic.
4. **Doubles the blocklist surface.** Today, renaming `report.txt` →
   `report.sh` could legitimately fail at the PUT step because the
   write-side extension blocklist (415) rejects `.sh`. With a native
   rename, the destination-extension policy is applied once on the
   server with full context, instead of being a side-effect of
   re-writing the bytes.

## Proposed contract

```
POST /api/v2/fs/{namespace}/{scope}/{src_path}?op=rename
Authorization: Bearer <jwt>
Content-Type: application/json

{"dest": "<dst_path>"}

→ 200 OK
  {"success": true, "src": "/<src_path>", "dst": "/<dst_path>"}
```

**Behavior:**

- Atomic at the row level. The storage layer (Postgres row update + S3
  key rename or none) commits in a single transaction.
- Cross-namespace and cross-scope renames return **409 Conflict** with
  `{"error": "cross_scope"}` — the FUSE layer translates to `EXDEV`
  and coreutils `mv` falls back to copy + unlink.
- Destination exists → **overwrite** (matches POSIX rename semantics).
  If the caller wants no-clobber, they set the standard FUSE
  `RENAME_NOREPLACE` flag, which the FUSE layer translates to a
  `?op=rename&noreplace=1` query param — server returns **409** if dst
  exists.
- Destination extension violates blocklist → **415**, identical to PUT.
- Source not found → **404**.
- Sandbox JWT scope mismatch (either `src_path` or `dst_path` outside
  scope) → **403**.
- Directories: same semantics as files (renames the prefix in the row's
  path column, or moves the S3 prefix if applicable). If directory
  rename is non-trivial server-side, **409 with `{"error":
  "directory_rename_unsupported"}`** is acceptable — FUSE falls back to
  EXDEV → recursive copy.

## Acceptance test (from the FUSE side)

After this endpoint ships, the FUSE layer's `rename` handler becomes:

```rust
self.client.rename(&ns, &scope, &src_path, &dst_path)
```

and the following must hold on a mounted `/reevofs/output/{chat_id}/`:

| #   | Command                                       | Expected                                  |
|-----|-----------------------------------------------|-------------------------------------------|
| 1   | `mv a.txt b.txt`                              | `a.txt` gone, `b.txt` has original bytes  |
| 2   | `sed -i 's/x/y/' f.txt`                       | exit 0; substitution applied              |
| 3   | `mv a.txt existing.txt` (overwrite)           | `a.txt` gone, `existing.txt` ← a's bytes  |
| 4   | `mv /reevofs/output/.../a /reevofs/skills/.../b` | mv falls back to copy+unlink (EXDEV)    |
| 5   | `mv a.txt /reevofs/skills/user/a.sh`          | 415 → EACCES at FUSE; mv reports refusal  |
| 6   | Server crashes between PUT-equivalent and commit | No partial state; src remains intact   |

## Suggested implementation hint

If files are stored as rows in a Postgres table keyed by
`(namespace, scope, path)` with a separate S3 reference for the bytes,
this is a **one-line path update**:

```sql
UPDATE agentfs_files
   SET path = $dst_path, updated_at = now()
 WHERE namespace = $ns AND scope = $scope AND path = $src_path
```

No S3 traffic, no byte transfer, atomic, preserves `created_at` and
`id`. For directories, the same `UPDATE` with `path LIKE $src_path/%`
and a path-prefix replace.

## Out of scope

- Cross-scope moves (intentional: scope is a permission boundary).
- Symlinks (AgentFS doesn't expose them today).
- Hard-linking (`POST ?op=link`) — separate ask if/when needed.

## Why we're asking

Without this, FUSE works for naive writes but breaks every editor and
every atomic-write library. The shim already works for these on the
host filesystem, so this gap only shows up in environments where the
shim can't be loaded (containers without `LD_PRELOAD`, non-glibc
runtimes, statically linked binaries that don't honor `LD_PRELOAD`).
FUSE is the long-tail compatibility path; rename is the last verb that
can't be cleanly emulated client-side.
