# reevofs-preload

An `LD_PRELOAD` shared library that transparently intercepts filesystem
syscalls for paths under `/reevofs/` and serves them via HTTP from the
Reevo API. All other paths pass through to the real libc with zero overhead.

## How it works

```
 Process (cat, python3, node, …)
    │
    ├─ open("/etc/passwd")          → libc (untouched)
    ├─ open("/reevofs/skill/FOO")   → HTTP GET → Reevo API → memfd → real FD
    └─ stat("/reevofs/skill/FOO")   → HTTP GET → synthetic stat buf
```

1. **LD_PRELOAD injection** – the container sets
   `LD_PRELOAD=/usr/local/lib/libreevofs_preload.so` so the dynamic linker
   loads this library before libc. Every call to `open`, `openat`, `stat`,
   etc. hits our shim first.

2. **Path matching** – the shim checks whether the path starts with the
   configurable prefix (default `/reevofs`). Non-matching paths are
   forwarded to the real libc function via `dlsym(RTLD_NEXT, …)` with no
   measurable overhead.

3. **HTTP fetch** – matching paths are translated to an API call:
   `/reevofs/<path>` → `GET {REEVO_API_URL}/api/v2/fs/{namespace}/{scope}/<path>`.
   The `reevofs-api` crate handles request construction and auth headers.

4. **memfd return** – file content is written to a `memfd_create` anonymous
   file descriptor (kernel-backed, no on-disk file). The FD is seeked back
   to offset 0 and returned to the caller. Because it's a real kernel FD,
   all subsequent operations (`read`, `fstat`, `lseek`, `mmap`, `close`)
   work natively with no further interception.

5. **stat synthesis** – `stat`/`lstat`/`fstatat` calls for `/reevofs/`
   paths return a synthetic stat buffer: regular file with mode 0644 and
   st_size set to the content length. Directory detection falls back to the
   `list_dir` API endpoint.

## Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `REEVO_API_URL` | **yes** | — | Base URL of the Reevo API (e.g. `https://api.reevo.ai`) |
| `REEVO_API_TOKEN` | no | `""` | JWT bearer token for API auth |
| `REEVO_USER_ID` | no | — | Sent as `x-reevo-user-id` header |
| `REEVO_ORG_ID` | no | — | Sent as `x-reevo-org-id` header |
| `REEVOFS_MOUNT_PREFIX` | no | `/reevofs` | Path prefix to intercept |
| `REEVOFS_NAMESPACE` | no | `skills` | API namespace parameter |
| `REEVOFS_SCOPE` | no | `org` | API scope parameter |

If `REEVO_API_URL` is unset, the shim disables itself entirely and all
calls pass through to libc.

## Hooked functions

The shim intercepts these libc symbols to cover all glibc/musl call paths
that programs and runtimes (CPython, Node, coreutils) use:

| Category | Functions |
|---|---|
| **Open** | `open`, `open64`, `openat`, `openat64` |
| **Stat** | `stat`, `stat64`, `lstat`, `lstat64`, `fstatat`, `fstatat64` |
| **Stat (glibc internal)** | `__xstat`, `__lxstat`, `__fxstatat` |
| **Access** | `access`, `faccessat` |

`fstat`, `read`, `write`, `close`, `lseek` are **not** hooked — they
operate on the real memfd kernel FD and work natively.

## Safety

- **Re-entrancy guard** – a thread-local flag prevents recursive hook
  calls (e.g. if the HTTP client itself calls `open`).
- **Lazy initialization** – config is loaded once on first intercepted
  call via `once_cell::Lazy`.
- **Pipe fallback** – if `memfd_create` is unavailable, content is
  served through a pipe (no lseek support, but read works).

## Building

```bash
# Native build
cargo build --release -p reevofs-preload

# Cross-compile for aarch64 (arm64) Linux containers
cross build --release --target aarch64-unknown-linux-gnu -p reevofs-preload
```

Output: `target/release/libreevofs_preload.so` (or under the cross target dir).

## Usage in Docker

```dockerfile
COPY libreevofs_preload.so /usr/local/lib/libreevofs_preload.so
```

The container runtime sets `LD_PRELOAD` and the `REEVO_*` env vars before
executing user commands. See the `agent-sandbox` Dockerfile in salestech-be.
