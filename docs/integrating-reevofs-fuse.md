# Integrating the reevofs FUSE mount

How to consume the `reevofs` binary from a downstream repo (agent
sandboxes, Daytona images, anywhere the LD_PRELOAD shim can't load).

## TL;DR

The binary you want is **`reevofs-linux-{amd64,arm64}`** from the
[GitHub releases page](https://github.com/ReevoAI/reevofs/releases).

There is no separate "reevofs-fuse" binary — the released `reevofs`
binary is built with `--features fuse` and exposes the `mount`
subcommand. If you've been looking for an asset with "fuse" in the
name, that's why it "doesn't exist."

```dockerfile
# Pick the latest release tag, e.g. v0.3.10
ARG REEVOFS_VERSION=v0.3.10
ARG TARGETARCH  # docker buildx provides this; "amd64" or "arm64"

RUN apt-get update && apt-get install -y fuse3 && rm -rf /var/lib/apt/lists/* \
 && curl -fsSL "https://github.com/ReevoAI/reevofs/releases/download/${REEVOFS_VERSION}/reevofs-linux-${TARGETARCH}" \
        -o /usr/local/bin/reevofs \
 && chmod +x /usr/local/bin/reevofs

# At container runtime, mount before launching the agent:
# reevofs mount /reevofs &
```

## Where the assets live

Every tagged release (since v0.3.2) attaches four files:

| Asset                                       | What it is                                  |
|---------------------------------------------|---------------------------------------------|
| `reevofs-linux-amd64`                       | CLI binary, **includes FUSE mount support** |
| `reevofs-linux-arm64`                       | Same, arm64                                 |
| `libreevofs_preload-linux-amd64.so`         | LD_PRELOAD shim (separate path, optional)   |
| `libreevofs_preload-linux-arm64.so`         | Same, arm64                                 |

The release workflow (`.github/workflows/release.yml`) builds the CLI
with `cargo build --release --features fuse`, so the `mount`
subcommand is in every published binary. There's no "preload-only" or
"fuse-only" variant.

Confirm with:

```
$ reevofs --help
Mount Reevo's AgentFS as a local filesystem

Commands:
  mount  Mount the filesystem (requires macFUSE or libfuse)
  ls     List files via the Reevo API (no FUSE required)
  cat    Read a file via the Reevo API (no FUSE required)
  write  Write a file via the Reevo API (no FUSE required)
```

## Container requirements

FUSE needs kernel cooperation. Inside Docker / Podman / Kubernetes pods:

| Requirement              | Why                                              |
|--------------------------|--------------------------------------------------|
| `--cap-add SYS_ADMIN`    | Required to call `mount(2)`                      |
| `--device /dev/fuse`     | The libfuse3 userspace talks to the kernel here  |
| `--security-opt apparmor=unconfined` | Some hosts; only if AppArmor blocks `/dev/fuse` |
| `fuse3` apt package      | Provides `fusermount3` (mount helper) + libfuse3 |

Daytona / Kubernetes equivalents:

```yaml
# Pod / container spec
securityContext:
  capabilities:
    add: ["SYS_ADMIN"]
volumeDevices:
  - name: fuse
    devicePath: /dev/fuse
volumes:
  - name: fuse
    hostPath:
      path: /dev/fuse
      type: CharDevice
```

If the runtime can't grant `SYS_ADMIN`, FUSE won't work and you must
fall back to the LD_PRELOAD shim instead.

## Configuration (env vars)

The mount layout is driven by env vars matching the
[shim conventions in executor.py](https://github.com/ReevoAI/salestech-be/blob/main/deploy/docker/agent-sandbox/executor.py):

| Env var                              | Value                            | Effect                                                       |
|--------------------------------------|----------------------------------|--------------------------------------------------------------|
| `REEVO_API_URL`                      | `https://api.reevo.ai`           | Backend base URL                                             |
| `REEVO_API_TOKEN`                    | sandbox JWT                      | Bearer token                                                 |
| `REEVO_USER_ID`                      | user uuid                        | `x-reevo-user-id` header                                     |
| `REEVO_ORG_ID`                       | org uuid                         | `x-reevo-org-id` header                                      |
| `REEVOFS_SCOPE_skills`               | `overlay`                        | Mount `/reevofs/skills/overlay/...`                          |
| `REEVOFS_SCOPE_output`               | `<chat_id>` UUID                 | Mount `/reevofs/output/<chat_id>/...`. **Unset → not mounted.** |
| `REEVOFS_SCOPE_chat_attachments`     | `user`                           | Mount `/reevofs/chat_attachments/user/...`. **Unset → not mounted.** |

A namespace whose `REEVOFS_SCOPE_*` env var is unset is **not mounted**
— matching shim behavior. If *none* of the three is set, the binary
falls back to a legacy `/skills/{system,org,user}` tree for standalone
dev use.

## Mount layout (important — differs from the shim)

The shim makes `/reevofs/output/file.txt` work transparently because it
translates the per-namespace scope env-var into the URL. The FUSE mount
exposes the scope as a directory level, so:

| Shim path                           | FUSE path                                        |
|-------------------------------------|--------------------------------------------------|
| `/reevofs/skills/foo`               | `/reevofs/skills/overlay/foo`                    |
| `/reevofs/output/report.json`       | `/reevofs/output/<chat_id>/report.json`          |
| `/reevofs/chat_attachments/img.png` | `/reevofs/chat_attachments/user/img.png`         |

If you need shim-compatible paths under FUSE, the recommended approach
is a thin bind-mount or a symlink set up after the mount:

```bash
mkdir -p /reevofs && reevofs mount /reevofs &
# wait for mount
until mountpoint -q /reevofs; do sleep 0.1; done

# Optional: alias the configured scopes so writes don't need the chat_id suffix
ln -sf "/reevofs/output/$REEVOFS_SCOPE_output" /reevofs-output
ln -sf "/reevofs/skills/$REEVOFS_SCOPE_skills" /reevofs-skills
```

## Mounting at container start

The mount must come up before the agent starts using `/reevofs/`. A
typical entrypoint:

```bash
#!/usr/bin/env bash
set -euo pipefail

mkdir -p /reevofs
reevofs mount /reevofs &
MOUNT_PID=$!

# Wait up to 10s for the mount to become ready
for _ in $(seq 1 50); do
    if mountpoint -q /reevofs; then break; fi
    sleep 0.2
done

if ! mountpoint -q /reevofs; then
    echo "FATAL: reevofs mount didn't come up" >&2
    exit 1
fi

# Clean shutdown
trap 'fusermount3 -u /reevofs 2>/dev/null || true; kill $MOUNT_PID 2>/dev/null || true' EXIT

# Launch the actual agent
exec /opt/agent/run.sh "$@"
```

The `reevofs mount` foreground binary holds the FUSE session for as
long as it runs. Send SIGTERM (or call `fusermount3 -u`) for a clean
unmount.

## Capability matrix (what works today)

Validated by `tests/fuse_integration_test.sh` (108 assertions, runs in
CI on every release tag):

| Capability                                          | Status |
|-----------------------------------------------------|--------|
| read / write / append / overwrite                   | ✅     |
| `cat`, `head`, `tail`, `tac`, `head -c`, `tail -c`  | ✅     |
| `truncate -s N` shrink                              | ✅     |
| `truncate -s N` grow (≤16 MiB, then EFBIG)          | ✅     |
| `sed -i`, `awk -i inplace`, Python `r+ + truncate`  | ✅     |
| `mv` same-namespace+scope                           | ✅ (GET+PUT+DELETE) |
| `mv` cross-namespace / cross-scope                  | ✅ (EXDEV → coreutils copy+unlink) |
| Directory rename                                    | ✅ (EXDEV → recursive copy+unlink) |
| `rm`, `mkdir -p`, `rmdir`                           | ✅     |
| `cp`, `cp -p`, `install -m`                         | ✅     |
| `chmod`, `chown`, `utimes`                          | ✅ (silent no-op; cp -p / sed -i don't abort) |
| `fsync`, `fdatasync`                                | ✅ (no-op, redundant for our design) |
| `df`, `du`                                          | ✅ (synthetic 1 TiB free) |
| `tar c`, `tar x`, `gzip`, `gunzip`, `base64`        | ✅     |
| `find`, `find -exec`, `xargs -0`, glob `*.txt`      | ✅     |
| Binary file round-trip (non-UTF-8 bytes)            | ✅     |
| UTF-8 / emoji filenames                             | ✅     |
| Long filenames (≤255 bytes)                         | ✅     |
| Files >256 KiB (inline → S3 boundary)               | ✅     |
| Read-only mmap (`mmap.ACCESS_READ`)                 | ✅     |
| Blocked extensions (`.bin`, `.exe`, etc.)           | ✅ → EACCES |
| `ln -s` (symlinks)                                  | ❌ → EPERM (we don't pretend to support) |

## Caveats and known limits

- **16 MiB truncate-grow cap.** `truncate -s 1G file` returns EFBIG.
  Doesn't affect agents writing actual content — that uses the write
  path, capped only by the backend's ~100 MiB PUT limit. The cap
  exists to prevent `fallocate -l 10G` from OOMing the FUSE daemon.
- **Rename is non-atomic.** Today's GET+PUT+DELETE has a window where
  both paths exist. A native rename endpoint is proposed in
  [`docs/superpowers/specs/2026-05-14-agentfs-native-rename-endpoint.md`](./superpowers/specs/2026-05-14-agentfs-native-rename-endpoint.md);
  the FUSE side will swap to one HTTP call when the BE ships it.
- **Write buffer is per-file.** The full bytes of a write live in
  memory until close. A 500 MiB write means 500 MiB resident in the
  FUSE daemon until flush.
- **Symlinks not supported.** `ln -s` fails. Agents almost never need
  this; if you do, please file an issue.
- **No inotify / fsnotify.** File-change notifications won't fire over
  the mount. Tail-following (`tail -f`) won't see new lines.

## Troubleshooting

| Symptom                                    | Likely cause                                          |
|--------------------------------------------|-------------------------------------------------------|
| `fuse: device not found`                   | Missing `--device /dev/fuse`                          |
| `fuse: failed to open /dev/fuse: Permission denied` | Missing `--cap-add SYS_ADMIN`                |
| `fusermount3: not found`                   | `apt-get install fuse3` missing in container image    |
| `cat: file: Permission denied`             | Extension is on the backend blocklist (.bin, .exe, .sh, .dll, .so, .dylib) |
| Mount up but namespace dir is missing      | `REEVOFS_SCOPE_<ns>` env var is unset                 |
| `Operation not permitted` on `ln -s`       | Expected — symlinks intentionally not supported       |
| `truncate: failed to truncate: File too large` | Hit the 16 MiB grow cap (see Caveats)             |

## Versioning

The new FUSE ops (rename, setattr, statfs, env-driven namespaces,
fsync) land in the next tagged release after this PR merges. **Use
≥v0.3.11 (or whatever the post-merge tag is) for full edit parity.**
v0.3.10 has FUSE support but lacks rename, setattr, and the env-var
namespace config — agents will hit EROFS / ENOSYS on common
operations.

Check the release tag's assets to confirm:

```
gh release view v0.3.11 --json assets --jq '.assets[].name'
```
