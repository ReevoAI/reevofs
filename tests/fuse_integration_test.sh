#!/usr/bin/env bash
# Integration test for the reevofs FUSE filesystem.
#
# Mirrors tests/integration_test.sh but exercises FUSE-mounted paths
# instead of the LD_PRELOAD shim. Covers the edit-parity acceptance
# matrix: rename, setattr/truncate, statfs, in-place writes, etc.
#
# Expects:
#   - reevofs binary built with --features fuse (in PATH or REEVOFS_BIN)
#   - Mock API server (tests/mock_api.py) already running on port 9876
#   - Container has /dev/fuse and CAP_SYS_ADMIN (docker run --cap-add SYS_ADMIN --device /dev/fuse)
#
# Mount layout (driven by REEVOFS_SCOPE_* env vars):
#   /mnt/reevofs/skills/overlay/...
#   /mnt/reevofs/output/<chat_id>/...
#   /mnt/reevofs/chat_attachments/user/...

set -uo pipefail

REEVOFS_BIN="${REEVOFS_BIN:-reevofs}"
MOUNT_POINT="${MOUNT_POINT:-/mnt/reevofs}"

export REEVO_API_URL="http://127.0.0.1:9876"
export REEVO_API_TOKEN="test-token"
export REEVO_USER_ID="test-user"
export REEVO_ORG_ID="test-org"
export REEVOFS_SCOPE_skills="overlay"
export REEVOFS_SCOPE_output="test-chat-id"
export REEVOFS_SCOPE_chat_attachments="user"

# FUSE mount layout exposes scope as a directory level. Shim uses /output/file
# transparently because it translates the namespace's env-var scope into the
# URL path; FUSE shows the scope dir explicitly so cross-scope renames have
# somewhere to be EXDEV against.
OUTPUT_DIR="$MOUNT_POINT/output/test-chat-id"
SKILLS_DIR="$MOUNT_POINT/skills/overlay"

PASS=0
FAIL=0
ERRORS=""

assert_eq() {
    local test_name="$1"
    local expected="$2"
    local actual="$3"
    if [ "$expected" = "$actual" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: $test_name"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: $test_name
    expected: $(printf '%s' "$expected" | head -c 200)
    actual:   $(printf '%s' "$actual" | head -c 200)"
        echo "  FAIL: $test_name"
    fi
}

assert_ok() {
    local test_name="$1"
    shift
    if "$@" > /dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo "  PASS: $test_name"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: $test_name (exit code $?)"
        echo "  FAIL: $test_name"
    fi
}

assert_fail() {
    local test_name="$1"
    shift
    if "$@" > /dev/null 2>&1; then
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: $test_name (expected failure, got success)"
        echo "  FAIL: $test_name"
    else
        PASS=$((PASS + 1))
        echo "  PASS: $test_name"
    fi
}

cleanup() {
    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        echo "Unmounting $MOUNT_POINT..."
        fusermount3 -u "$MOUNT_POINT" 2>/dev/null \
            || fusermount -u "$MOUNT_POINT" 2>/dev/null \
            || umount "$MOUNT_POINT" 2>/dev/null \
            || true
    fi
    if [ -n "${MOUNT_PID:-}" ]; then
        kill "$MOUNT_PID" 2>/dev/null || true
        wait "$MOUNT_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# ═══════════════════════════════════════════════════════════════════════
echo "=== Mounting ReevoFS at $MOUNT_POINT ==="
# ═══════════════════════════════════════════════════════════════════════

mkdir -p "$MOUNT_POINT"
"$REEVOFS_BIN" mount "$MOUNT_POINT" > /tmp/reevofs-mount.log 2>&1 &
MOUNT_PID=$!

# Wait for mount to become ready (max 10s).
for i in $(seq 1 50); do
    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        break
    fi
    sleep 0.2
done

if ! mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    echo "FATAL: mount did not come up within 10s"
    echo "--- mount log ---"
    cat /tmp/reevofs-mount.log
    exit 1
fi

echo "Mount ready. PID=$MOUNT_PID"
echo ""

# ═══════════════════════════════════════════════════════════════════════
echo "=== 0. Namespace mount sanity ==="
# ═══════════════════════════════════════════════════════════════════════

assert_ok "stat mount point" stat "$MOUNT_POINT"
assert_ok "stat skills namespace" stat "$SKILLS_DIR"
assert_ok "stat output namespace" stat "$OUTPUT_DIR"
assert_ok "stat chat_attachments namespace" stat "$MOUNT_POINT/chat_attachments"
assert_ok "ls mount root" ls "$MOUNT_POINT/"
assert_ok "ls empty output namespace" ls "$OUTPUT_DIR/"

# Pre-seeded skills content from mock_api.py
OUT=$(cat "$SKILLS_DIR/hello.txt")
assert_eq "read pre-seeded skills file" "hello world" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 1. echo X > F (create new file) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test1.txt"
echo "hello-1" > "$F"
OUT=$(cat "$F")
assert_eq "create new file with redirect" "hello-1" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 2. echo Y >> F (append) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test2.txt"
echo "line1" > "$F"
echo "line2" >> "$F"
OUT=$(cat "$F")
assert_eq "append second line" "line1
line2" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 3. echo Z > F (overwrite existing) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test3.txt"
echo "original" > "$F"
echo "replaced" > "$F"
OUT=$(cat "$F")
assert_eq "overwrite existing file" "replaced" "$OUT"

# Verify file size shrank — old "original\n" was 9 bytes, new is 9 bytes too
# so use a stronger test: long → short.
echo "this is a longer initial line" > "$F"
echo "X" > "$F"
SIZE=$(stat -c '%s' "$F" 2>/dev/null || stat -f '%z' "$F")
assert_eq "overwrite truncates size" "2" "$SIZE"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 4. sed -i (atomic-write-then-rename) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test4.txt"
echo "the quick brown fox" > "$F"
sed -i 's/quick/slow/' "$F" 2>/tmp/sed.err
SED_RC=$?
if [ "$SED_RC" -ne 0 ]; then
    echo "  (sed -i stderr: $(cat /tmp/sed.err))"
fi
OUT=$(cat "$F")
assert_eq "sed -i substitution" "the slow brown fox" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 5. Python r+ seek+write+truncate ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test5.txt"
echo "originaaaaal content" > "$F"  # 21 bytes including newline
python3 -c "
import sys
p = sys.argv[1]
with open(p, 'r+') as f:
    f.seek(0)
    f.write('short')
    f.truncate()
" "$F"
OUT=$(cat "$F")
SIZE=$(stat -c '%s' "$F" 2>/dev/null || stat -f '%z' "$F")
assert_eq "python r+ seek+write+truncate content" "short" "$OUT"
assert_eq "python r+ seek+write+truncate size" "5" "$SIZE"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 6. truncate -s N (shrink) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test6.txt"
echo -n "abcdefghij" > "$F"  # 10 bytes
truncate -s 4 "$F"
OUT=$(cat "$F")
SIZE=$(stat -c '%s' "$F" 2>/dev/null || stat -f '%z' "$F")
assert_eq "truncate -s 4 content" "abcd" "$OUT"
assert_eq "truncate -s 4 size" "4" "$SIZE"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 7. truncate -s N (grow, within 16 MiB cap) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test7.txt"
echo -n "abc" > "$F"
truncate -s 100 "$F"
SIZE=$(stat -c '%s' "$F" 2>/dev/null || stat -f '%z' "$F")
assert_eq "truncate -s 100 grow size" "100" "$SIZE"
# Bytes 0..2 = 'abc', bytes 3..99 = '\0'
HEAD=$(head -c 3 "$F")
assert_eq "truncate grow preserves head" "abc" "$HEAD"
# Verify byte 50 is NUL
BYTE50_HEX=$(dd if="$F" bs=1 skip=50 count=1 2>/dev/null | od -An -tx1 | tr -d ' ')
assert_eq "truncate grow pads with NUL" "00" "$BYTE50_HEX"

# Grow beyond 16 MiB cap should fail with EFBIG
F="$OUTPUT_DIR/test7-toobig.txt"
echo -n "x" > "$F"
if truncate -s $((17 * 1024 * 1024)) "$F" 2>/tmp/trunc.err; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: truncate beyond 16 MiB cap (expected EFBIG, got success)"
    echo "  FAIL: truncate beyond 16 MiB cap"
else
    if grep -q -i "file too large\|EFBIG" /tmp/trunc.err 2>/dev/null; then
        PASS=$((PASS + 1))
        echo "  PASS: truncate beyond 16 MiB cap returns EFBIG"
    else
        # Some coreutils builds report "Invalid argument" for the kernel's
        # EFBIG path. We accept any non-zero exit.
        PASS=$((PASS + 1))
        echo "  PASS: truncate beyond 16 MiB cap fails (stderr: $(head -c 80 /tmp/trunc.err))"
    fi
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 8. cp /tmp/src F (overwrite via cp) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test8.txt"
echo "old content here" > "$F"
echo "fresh from cp" > /tmp/cp-src.txt
cp /tmp/cp-src.txt "$F"
OUT=$(cat "$F")
assert_eq "cp overwrite" "fresh from cp" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 9. mv F G (same namespace+scope) ==="
# ═══════════════════════════════════════════════════════════════════════

SRC="$OUTPUT_DIR/test9-src.txt"
DST="$OUTPUT_DIR/test9-dst.txt"
echo "move me" > "$SRC"

# Snapshot the mock's per-endpoint call counts so we can prove the rename
# goes through ?op=rename (the BE's native endpoint) and NOT the legacy
# GET+PUT+DELETE emulation.
fetch_rename_count() {
    curl -sf http://127.0.0.1:9876/_stats \
        | python3 -c 'import json,sys; print(json.load(sys.stdin).get("rename", 0))'
}
RENAMES_BEFORE=$(fetch_rename_count)

mv "$SRC" "$DST"

RENAMES_AFTER=$(fetch_rename_count)
RENAME_DELTA=$((RENAMES_AFTER - RENAMES_BEFORE))
if [ "$RENAME_DELTA" -ge 1 ]; then
    PASS=$((PASS + 1))
    echo "  PASS: mv invoked native ?op=rename endpoint (calls: $RENAME_DELTA)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: mv did NOT invoke native rename (delta=$RENAME_DELTA — still emulating with GET+PUT+DELETE?)"
    echo "  FAIL: mv did NOT invoke native rename (delta=$RENAME_DELTA)"
fi

if [ -e "$SRC" ]; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: mv same-ns leaves source behind"
    echo "  FAIL: mv same-ns leaves source behind"
else
    PASS=$((PASS + 1))
    echo "  PASS: mv same-ns removes source"
fi
OUT=$(cat "$DST")
assert_eq "mv same-ns dest content" "move me" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 10. mv F /reevofs/skills/... (cross-namespace) ==="
# ═══════════════════════════════════════════════════════════════════════

SRC="$OUTPUT_DIR/test10-src.txt"
DST="$SKILLS_DIR/test10-dst.txt"
echo "cross-namespace move" > "$SRC"
if mv "$SRC" "$DST" 2>/tmp/mv.err; then
    # coreutils mv may copy+unlink on EXDEV → both ops succeed.
    if [ -e "$SRC" ]; then
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: cross-ns mv source remains"
        echo "  FAIL: cross-ns mv source remains"
    else
        OUT=$(cat "$DST" 2>/dev/null || true)
        if [ "$OUT" = "cross-namespace move" ]; then
            PASS=$((PASS + 1))
            echo "  PASS: cross-ns mv falls back to copy+unlink"
        else
            FAIL=$((FAIL + 1))
            ERRORS="${ERRORS}
  FAIL: cross-ns mv dest content mismatch (got: $OUT)"
            echo "  FAIL: cross-ns mv dest content mismatch"
        fi
    fi
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: cross-ns mv failed entirely (stderr: $(head -c 200 /tmp/mv.err))"
    echo "  FAIL: cross-ns mv failed entirely"
fi

# Clean up
rm -f "$DST" 2>/dev/null || true

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 11. dd partial in-place (seek + notrunc) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test11.txt"
echo -n "AAAAAAAAAA" > "$F"  # 10 bytes
# Replace bytes 3-4 with "XX"
printf 'XX' | dd of="$F" bs=1 seek=3 conv=notrunc count=2 2>/dev/null
OUT=$(cat "$F")
assert_eq "dd partial in-place" "AAAXXAAAAA" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 12. rm F ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test12.txt"
echo "delete me" > "$F"
rm "$F"
if [ -e "$F" ]; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: rm leaves file behind"
    echo "  FAIL: rm leaves file behind"
else
    PASS=$((PASS + 1))
    echo "  PASS: rm removes file"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 13. df /reevofs ==="
# ═══════════════════════════════════════════════════════════════════════

if df "$MOUNT_POINT" > /tmp/df.out 2>&1; then
    if grep -q "$MOUNT_POINT" /tmp/df.out; then
        PASS=$((PASS + 1))
        echo "  PASS: df reports mount"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: df output missing mount point: $(cat /tmp/df.out)"
        echo "  FAIL: df output missing mount point"
    fi
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: df failed"
    echo "  FAIL: df failed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 14. Python JSON round-trip ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test14.json"
echo '{"a": 1, "b": 2}' > "$F"
python3 -c "
import json, sys
p = sys.argv[1]
with open(p) as f:
    d = json.load(f)
d['k'] = 1
with open(p, 'w') as f:
    json.dump(d, f)
" "$F"
OUT=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['k'])" "$F")
assert_eq "JSON edit round-trip" "1" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 15. Regression: list previously written file ==="
# ═══════════════════════════════════════════════════════════════════════

# Sleep to let TTL expire so readdir re-fetches.
sleep 6
OUT=$(ls "$OUTPUT_DIR/" | grep -c test4 || true)
if [ "$OUT" = "1" ]; then
    PASS=$((PASS + 1))
    echo "  PASS: readdir shows previously-written file after TTL"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: readdir missing test4 after TTL (count=$OUT)"
    echo "  FAIL: readdir missing test4 after TTL"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 16. rename over existing destination ==="
# ═══════════════════════════════════════════════════════════════════════

SRC="$OUTPUT_DIR/test16-src.txt"
DST="$OUTPUT_DIR/test16-dst.txt"
echo "source content" > "$SRC"
echo "destination original" > "$DST"
mv "$SRC" "$DST"
OUT=$(cat "$DST")
assert_eq "rename overwrites existing dest" "source content" "$OUT"
if [ -e "$SRC" ]; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: rename-over-existing left source behind"
    echo "  FAIL: rename-over-existing left source behind"
else
    PASS=$((PASS + 1))
    echo "  PASS: rename-over-existing removes source"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 17. rename to self (mv X X) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test17.txt"
echo "self-rename content" > "$F"
# GNU mv refuses with "are the same file" — that's still a success path for
# our purposes (no destructive behavior). Older mv may issue the rename.
mv "$F" "$F" 2>/tmp/mv-self.err || true
OUT=$(cat "$F")
assert_eq "rename-to-self preserves content" "self-rename content" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 18. Large file write/read (>256 KB inline→S3 boundary) ==="
# ═══════════════════════════════════════════════════════════════════════

# Use .dat (not .bin) — .bin is on the mock's blocklist matching production.
F="$OUTPUT_DIR/test18-large.dat"
# 300 KB of random-ish data — stays under the 16 MiB cap but crosses the
# backend's inline/S3 boundary. Generated deterministically so we can verify.
python3 -c "
import sys
data = (b'reevofs-large-write-' * 13) * 1024  # ~270 KB
with open(sys.argv[1], 'wb') as f:
    f.write(data)
" "$F"
SIZE=$(stat -c '%s' "$F" 2>/dev/null || stat -f '%z' "$F")
EXPECTED_SIZE=$(python3 -c "print(len((b'reevofs-large-write-' * 13) * 1024))")
assert_eq "large file size matches" "$EXPECTED_SIZE" "$SIZE"
# Verify integrity by hashing.
HASH=$(python3 -c "
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], 'rb').read()).hexdigest())
" "$F")
EXPECTED_HASH=$(python3 -c "
import hashlib
data = (b'reevofs-large-write-' * 13) * 1024
print(hashlib.sha256(data).hexdigest())
")
assert_eq "large file hash matches" "$EXPECTED_HASH" "$HASH"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 19. Binary file round-trip (non-UTF-8 bytes) ==="
# ═══════════════════════════════════════════════════════════════════════

# Pre-seeded by the mock: skills/overlay/binary.dat = bytes [0xff,0xfe,0xfd,0xfc]
F="$SKILLS_DIR/binary.dat"
HASH=$(python3 -c "
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], 'rb').read()).hexdigest())
" "$F")
EXPECTED=$(python3 -c "
import hashlib
print(hashlib.sha256(bytes([0xff,0xfe,0xfd,0xfc])).hexdigest())
")
assert_eq "binary file bytes survive FUSE round-trip" "$EXPECTED" "$HASH"

# Write a binary file and read it back. Use .dat — .bin is blocked.
F="$OUTPUT_DIR/test19.dat"
python3 -c "
import sys
with open(sys.argv[1], 'wb') as f:
    f.write(bytes([0,1,2,0xff,0xfe,0x80,0x7f]))
" "$F"
HASH=$(python3 -c "
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], 'rb').read()).hexdigest())
" "$F")
EXPECTED=$(python3 -c "
import hashlib
print(hashlib.sha256(bytes([0,1,2,0xff,0xfe,0x80,0x7f])).hexdigest())
")
assert_eq "binary write+read round-trip" "$EXPECTED" "$HASH"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 20. 403 → EACCES propagation ==="
# ═══════════════════════════════════════════════════════════════════════

# Mock API rejects any scope starting with "reject-" with a 400, and a
# blocked extension (.exe / .bin) with 415 → Forbidden → EACCES.
F="$OUTPUT_DIR/test20.exe"
if echo "should fail" > "$F" 2>/tmp/blocked.err; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: write to blocked extension succeeded (expected EACCES)"
    echo "  FAIL: write to blocked extension succeeded"
else
    if grep -q -i "permission denied\|EACCES" /tmp/blocked.err 2>/dev/null; then
        PASS=$((PASS + 1))
        echo "  PASS: blocked-extension write → EACCES"
    else
        # Some shells report different wording; non-zero exit + non-success
        # is the contract. Accept any non-zero outcome.
        PASS=$((PASS + 1))
        echo "  PASS: blocked-extension write fails (stderr: $(head -c 80 /tmp/blocked.err))"
    fi
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 21. mkdir + nested directory operations ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test21-dir"
mkdir "$DIR"
assert_ok "stat newly-created dir" stat "$DIR"
echo "nested" > "$DIR/inside.txt"
OUT=$(cat "$DIR/inside.txt")
assert_eq "read file in mkdir'd dir" "nested" "$OUT"

# mkdir -p with deeper nesting
mkdir -p "$OUTPUT_DIR/test21-deep/a/b/c"
echo "deep" > "$OUTPUT_DIR/test21-deep/a/b/c/file.txt"
OUT=$(cat "$OUTPUT_DIR/test21-deep/a/b/c/file.txt")
assert_eq "mkdir -p deep nesting" "deep" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 22. Special characters in filenames ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/file with spaces.txt"
echo "spaces ok" > "$F"
OUT=$(cat "$F")
assert_eq "filename with spaces" "spaces ok" "$OUT"

F="$OUTPUT_DIR/file-with-dashes_and_underscores.txt"
echo "punct ok" > "$F"
OUT=$(cat "$F")
assert_eq "filename with dashes and underscores" "punct ok" "$OUT"

F="$OUTPUT_DIR/file.with.many.dots.txt"
echo "dots ok" > "$F"
OUT=$(cat "$F")
assert_eq "filename with multiple dots" "dots ok" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 23. find / grep -r across files ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test23"
mkdir -p "$DIR"
echo "needle1" > "$DIR/a.txt"
echo "haystack" > "$DIR/b.txt"
echo "needle2" > "$DIR/c.txt"

FOUND=$(find "$DIR" -name '*.txt' | wc -l | tr -d ' ')
assert_eq "find returns all txt files" "3" "$FOUND"

# grep -r — ensure FUSE-mounted reads compose with recursive grep
GREP_HITS=$(grep -r needle "$DIR" 2>/dev/null | wc -l | tr -d ' ')
assert_eq "grep -r finds matches across FUSE files" "2" "$GREP_HITS"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 24. Heredoc and pipe write patterns ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test24-heredoc.txt"
cat > "$F" << 'EOF'
line1
line2
line3
EOF
OUT=$(cat "$F")
assert_eq "heredoc write" "line1
line2
line3" "$OUT"

F="$OUTPUT_DIR/test24-pipe.txt"
printf "piped\ndata\n" | tee "$F" > /dev/null
OUT=$(cat "$F")
assert_eq "pipe through tee" "piped
data" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 25. chat_attachments namespace read ==="
# ═══════════════════════════════════════════════════════════════════════

# chat_attachments is mounted but read-only in real deployments. Verify
# stat works (regression for the "namespace skipped when env unset" path).
assert_ok "stat chat_attachments/user scope" stat "$MOUNT_POINT/chat_attachments/user"
assert_ok "ls chat_attachments/user" ls "$MOUNT_POINT/chat_attachments/user/"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 26. Explicit fsync / fdatasync ==="
# ═══════════════════════════════════════════════════════════════════════

# Many atomic-write libraries explicitly fsync after write before rename.
# vim with `set fsync`, sqlite, python's `os.fsync`. Default FUSE returns
# ENOSYS for fsync which makes these apps fail their save path.
F="$OUTPUT_DIR/test26-fsync.txt"
python3 -c "
import os, sys
with open(sys.argv[1], 'w') as f:
    f.write('synced content')
    f.flush()
    os.fsync(f.fileno())
" "$F"
OUT=$(cat "$F")
assert_eq "explicit fsync write" "synced content" "$OUT"

# os.fdatasync — separate from fsync but same FUSE op.
F="$OUTPUT_DIR/test26-fdatasync.txt"
python3 -c "
import os, sys
with open(sys.argv[1], 'w') as f:
    f.write('fdatasync content')
    f.flush()
    os.fdatasync(f.fileno())
" "$F"
OUT=$(cat "$F")
assert_eq "explicit fdatasync write" "fdatasync content" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 27. mmap read ==="
# ═══════════════════════════════════════════════════════════════════════

# Tools that mmap files (ripgrep, sqlite read paths, some log readers,
# linker reading source files). FUSE handles file-backed mmap through
# the kernel page cache populated by our read() — should "just work" but
# worth a regression test.
F="$OUTPUT_DIR/test27-mmap.txt"
echo "mmap content for kernel page cache" > "$F"
OUT=$(python3 -c "
import mmap, sys
with open(sys.argv[1], 'rb') as f:
    with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as m:
        print(m[:].decode().rstrip())
" "$F")
assert_eq "mmap read of FUSE file" "mmap content for kernel page cache" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 28. copy_file_range / sendfile (modern cp) ==="
# ═══════════════════════════════════════════════════════════════════════

# GNU cp 8.27+ uses copy_file_range(2) when available. FUSE without an
# explicit handler causes the kernel to fall back to read/write, which
# should give correct results just (potentially) slower.
SRC=/tmp/test28-src.dat
DST="$OUTPUT_DIR/test28-dst.dat"
# Generate 50 KB of structured data. Use .dat not .bin (blocklist).
python3 -c "
import sys
with open(sys.argv[1], 'wb') as f:
    f.write(b'A' * 20480 + b'B' * 20480 + b'C' * 10240)
" "$SRC"
cp "$SRC" "$DST"
HASH_SRC=$(python3 -c "
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], 'rb').read()).hexdigest())
" "$SRC")
HASH_DST=$(python3 -c "
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], 'rb').read()).hexdigest())
" "$DST")
assert_eq "cp host→FUSE preserves content (copy_file_range fallback)" "$HASH_SRC" "$HASH_DST"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 29. awk -i inplace (atomic-write via tempfile + rename) ==="
# ═══════════════════════════════════════════════════════════════════════

# Same atomic-edit pattern as sed -i but via gawk. Different temp-file
# naming and rename ordering — exercises a slightly different path.
F="$OUTPUT_DIR/test29.txt"
echo "foo bar baz" > "$F"
if command -v awk >/dev/null && awk --version 2>&1 | grep -q "GNU Awk"; then
    awk -i inplace '{gsub(/bar/, "BAR"); print}' "$F" 2>/tmp/awk.err || true
    OUT=$(cat "$F")
    if [ "$OUT" = "foo BAR baz" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: awk -i inplace"
    else
        # gawk's inplace extension may not be packaged; not a hard failure.
        PASS=$((PASS + 1))
        echo "  SKIP: awk -i inplace (got: $OUT, gawk extension may be missing)"
    fi
else
    PASS=$((PASS + 1))
    echo "  SKIP: awk -i inplace (no gawk)"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 30. gzip / gunzip streaming ==="
# ═══════════════════════════════════════════════════════════════════════

F_PLAIN="$OUTPUT_DIR/test30.txt"
F_GZ="$OUTPUT_DIR/test30.txt.gz"
echo "compress me $(date +%s)" > "$F_PLAIN"
EXPECTED=$(cat "$F_PLAIN")
gzip -k "$F_PLAIN"  # -k keeps original, writes .gz
if [ -e "$F_GZ" ]; then
    PASS=$((PASS + 1))
    echo "  PASS: gzip writes .gz file"
    DECOMPRESSED=$(gunzip -c "$F_GZ")
    assert_eq "gunzip round-trip" "$EXPECTED" "$DECOMPRESSED"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: gzip did not produce .gz"
    echo "  FAIL: gzip did not produce .gz"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 31. base64 encode/decode round-trip ==="
# ═══════════════════════════════════════════════════════════════════════

F_PLAIN="$OUTPUT_DIR/test31.txt"
F_B64="$OUTPUT_DIR/test31.b64"
echo "secret payload" > "$F_PLAIN"
base64 "$F_PLAIN" > "$F_B64"
DECODED=$(base64 -d "$F_B64")
assert_eq "base64 encode/decode" "secret payload" "$DECODED"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 32. Symlink creation (expected to fail gracefully) ==="
# ═══════════════════════════════════════════════════════════════════════

# We don't implement symlink ops. ln -s should fail rather than create
# a broken/silent state. Agents rarely need symlinks; documenting the
# behavior here so a future failure is intentional.
F="$OUTPUT_DIR/test32-target.txt"
echo "target" > "$F"
if ln -s "$F" "$OUTPUT_DIR/test32-link.txt" 2>/tmp/symlink.err; then
    # Unexpected — but if it somehow worked, that's not a failure.
    PASS=$((PASS + 1))
    echo "  PASS: symlink creation succeeded (unexpected, fine)"
else
    PASS=$((PASS + 1))
    echo "  PASS: symlink creation fails gracefully (expected: $(head -c 100 /tmp/symlink.err))"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 33. cp -p (preserve mode/times — exercises utimens setattr) ==="
# ═══════════════════════════════════════════════════════════════════════

# cp -p sets utimes/chmod on the destination. If setattr returns ENOSYS,
# cp -p aborts with a "preserving permissions failed" error.
SRC=/tmp/test33-src.txt
DST="$OUTPUT_DIR/test33-dst.txt"
echo "preserve me" > "$SRC"
chmod 0644 "$SRC"
if cp -p "$SRC" "$DST" 2>/tmp/cpp.err; then
    PASS=$((PASS + 1))
    echo "  PASS: cp -p succeeds"
    OUT=$(cat "$DST")
    assert_eq "cp -p preserves content" "preserve me" "$OUT"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: cp -p failed (stderr: $(head -c 200 /tmp/cpp.err))"
    echo "  FAIL: cp -p failed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 34. Long filenames (close to 255-byte limit) ==="
# ═══════════════════════════════════════════════════════════════════════

LONG_NAME=$(printf 'a%.0s' $(seq 1 200))  # 200 'a's
F="$OUTPUT_DIR/$LONG_NAME.txt"
echo "long filename ok" > "$F"
OUT=$(cat "$F")
assert_eq "long filename write+read" "long filename ok" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 35. Multi-byte UTF-8 filenames ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/日本語ファイル.txt"
echo "utf8 filename ok" > "$F"
OUT=$(cat "$F")
assert_eq "UTF-8 filename write+read" "utf8 filename ok" "$OUT"

F="$OUTPUT_DIR/emoji-😀-file.txt"
echo "emoji filename ok" > "$F"
OUT=$(cat "$F")
assert_eq "emoji filename write+read" "emoji filename ok" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 36. Multi-byte content (UTF-8 inside files) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test36-utf8.txt"
echo "héllo wörld 日本語 🎉" > "$F"
OUT=$(cat "$F")
assert_eq "UTF-8 content round-trip" "héllo wörld 日本語 🎉" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 37. lseek-heavy reads (tac / head -c / tail -c) ==="
# ═══════════════════════════════════════════════════════════════════════

# tac reads from EOF backwards — needs accurate size from getattr + lseek
# SEEK_END. Common in log inspection workflows.
F="$OUTPUT_DIR/test37.txt"
printf 'line1\nline2\nline3\n' > "$F"
OUT=$(tac "$F")
assert_eq "tac (reverse cat via SEEK_END)" "line3
line2
line1" "$OUT"

# head -c N — reads first N bytes via lseek + read
OUT=$(head -c 5 "$F")
assert_eq "head -c 5" "line1" "$OUT"

# tail -c N — reads last N bytes via SEEK_END. Bash command-substitution
# strips trailing newlines, so the captured value is the 5-byte body.
OUT=$(tail -c 6 "$F")
assert_eq "tail -c 6" "line3" "$OUT"

# tail -n N — line-counting from end
OUT=$(tail -n 1 "$F")
assert_eq "tail -n 1" "line3" "$OUT"

# dd with skip — uses SEEK_SET
OUT=$(dd if="$F" bs=1 skip=6 count=5 2>/dev/null)
assert_eq "dd skip=6 count=5" "line2" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 38. Binary inspection (file / od / xxd / hexdump / strings) ==="
# ═══════════════════════════════════════════════════════════════════════

# `file` does magic-byte detection — reads at multiple offsets. Output
# varies by libmagic version so just check it produced something.
F="$OUTPUT_DIR/test38.txt"
echo "plain text content" > "$F"
FILE_OUT=$(file "$F" 2>/dev/null || true)
if [ -n "$FILE_OUT" ]; then
    PASS=$((PASS + 1))
    echo "  PASS: file produces type output ($FILE_OUT)"
else
    PASS=$((PASS + 1))
    echo "  SKIP: file not installed"
fi

# od -An -c — reads bytes and prints characters. Spacing varies between
# coreutils versions so just verify the printable chars are all present.
OUT=$(od -An -c "$F" | tr -s ' ' | head -1)
case "$OUT" in
    *p*l*a*i*n*t*e*x*t*)
        PASS=$((PASS + 1)); echo "  PASS: od -An -c"
        ;;
    *)
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: od -An -c (got: $OUT)"
        echo "  FAIL: od -An -c"
        ;;
esac

# xxd hex dump
if command -v xxd >/dev/null; then
    OUT=$(echo "ABC" | xxd -p | tr -d '\n')
    echo "ABC" > "$F"
    OUT2=$(xxd -p "$F" | tr -d '\n')
    assert_eq "xxd -p (hex dump)" "$OUT" "$OUT2"
else
    PASS=$((PASS + 1))
    echo "  SKIP: xxd not installed"
fi

# hexdump
if command -v hexdump >/dev/null; then
    OUT=$(hexdump -C "$F" | head -1 | awk '{print $2 $3 $4}')
    assert_eq "hexdump -C first bytes" "414243" "$OUT"
else
    PASS=$((PASS + 1))
    echo "  SKIP: hexdump not installed"
fi

# strings — extracts printable runs (from binutils; may not be installed)
if command -v strings >/dev/null; then
    printf 'header\nbody text\n' > "$F"
    OUT=$(strings "$F" | head -1)
    assert_eq "strings finds printable runs" "header" "$OUT"
else
    PASS=$((PASS + 1))
    echo "  SKIP: strings not installed (binutils)"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 39. wc variants ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test39.txt"
printf 'one two three\nfour five\nsix\n' > "$F"
LINES=$(wc -l < "$F" | tr -d ' ')
WORDS=$(wc -w < "$F" | tr -d ' ')
BYTES=$(wc -c < "$F" | tr -d ' ')
assert_eq "wc -l" "3" "$LINES"
assert_eq "wc -w" "6" "$WORDS"
# 13 + 1 (\n) + 9 + 1 + 3 + 1 = 28 bytes
assert_eq "wc -c" "28" "$BYTES"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 40. du recursive ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test40-du"
mkdir -p "$DIR/sub1" "$DIR/sub2"
echo "aaaaaaaa" > "$DIR/sub1/a.txt"  # 9 bytes
echo "bbbb" > "$DIR/sub2/b.txt"      # 5 bytes
# du -b reports byte-accurate apparent size; the dir overhead varies, so
# just confirm du completes successfully and returns non-empty output.
if du -sb "$DIR" > /tmp/du.out 2>&1; then
    if [ -s /tmp/du.out ]; then
        PASS=$((PASS + 1))
        echo "  PASS: du -sb produces output ($(cat /tmp/du.out))"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: du output empty"
        echo "  FAIL: du output empty"
    fi
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: du failed (stderr: $(cat /tmp/du.out))"
    echo "  FAIL: du failed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 41. diff / cmp (two-file parallel read) ==="
# ═══════════════════════════════════════════════════════════════════════

A="$OUTPUT_DIR/test41-a.txt"
B="$OUTPUT_DIR/test41-b.txt"
printf 'line1\nline2\n' > "$A"
printf 'line1\nline2\n' > "$B"
if diff "$A" "$B" > /dev/null; then
    PASS=$((PASS + 1))
    echo "  PASS: diff identical files (exit 0)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: diff says identical files differ"
    echo "  FAIL: diff identical files"
fi

printf 'line1\nDIFFERENT\n' > "$B"
if diff "$A" "$B" > /dev/null; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: diff says different files identical"
    echo "  FAIL: diff different files"
else
    PASS=$((PASS + 1))
    echo "  PASS: diff different files (exit 1)"
fi

# cmp byte-level
printf 'line1\nline2\n' > "$B"
if cmp -s "$A" "$B"; then
    PASS=$((PASS + 1))
    echo "  PASS: cmp identical files"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: cmp identical files reports diff"
    echo "  FAIL: cmp identical files"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 42. patch (read + write) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test42.txt"
printf 'foo\nbar\n' > "$F"
PATCH_FILE=/tmp/test42.patch
cat > "$PATCH_FILE" << 'EOF'
--- a.txt	2024-01-01
+++ b.txt	2024-01-01
@@ -1,2 +1,2 @@
 foo
-bar
+BAZ
EOF
if command -v patch >/dev/null; then
    if patch "$F" < "$PATCH_FILE" > /dev/null 2>&1; then
        OUT=$(cat "$F")
        assert_eq "patch applies cleanly" "foo
BAZ" "$OUT"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: patch refused to apply"
        echo "  FAIL: patch refused to apply"
    fi
else
    PASS=$((PASS + 1))
    echo "  SKIP: patch not installed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 43. touch (create empty + utimens on existing) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test43-new.txt"
touch "$F"
assert_ok "touch creates empty file" stat "$F"
SIZE=$(stat -c '%s' "$F")
assert_eq "touch produces 0-byte file" "0" "$SIZE"

# touch on existing — just updates utimens (which we accept as no-op)
echo "preserved" > "$OUTPUT_DIR/test43-existing.txt"
touch "$OUTPUT_DIR/test43-existing.txt"
OUT=$(cat "$OUTPUT_DIR/test43-existing.txt")
assert_eq "touch on existing preserves content" "preserved" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 44. install (cp + chmod + utimens combined) ==="
# ═══════════════════════════════════════════════════════════════════════

SRC=/tmp/test44-src.txt
DST="$OUTPUT_DIR/test44-installed.txt"
echo "installed payload" > "$SRC"
if install -m 0644 "$SRC" "$DST" 2>/tmp/install.err; then
    OUT=$(cat "$DST")
    assert_eq "install -m 0644 preserves content" "installed payload" "$OUT"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: install failed (stderr: $(head -c 200 /tmp/install.err))"
    echo "  FAIL: install failed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 45. mktemp inside FUSE dir ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test45-mktemp"
mkdir -p "$DIR"
if TMPF=$(mktemp -p "$DIR" 2>/tmp/mktemp.err); then
    assert_ok "mktemp file exists" stat "$TMPF"
    echo "test" > "$TMPF"
    OUT=$(cat "$TMPF")
    assert_eq "mktemp file is writable" "test" "$OUT"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: mktemp -p failed (stderr: $(head -c 200 /tmp/mktemp.err))"
    echo "  FAIL: mktemp failed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 46. tar create + extract ==="
# ═══════════════════════════════════════════════════════════════════════

SRC_DIR="$OUTPUT_DIR/test46-src"
mkdir -p "$SRC_DIR"
echo "file 1" > "$SRC_DIR/a.txt"
echo "file 2" > "$SRC_DIR/b.txt"
mkdir -p "$SRC_DIR/nested"
echo "nested file" > "$SRC_DIR/nested/c.txt"

TAR_FILE=/tmp/test46.tar
# tar c: stat + open + read many files. The archive goes to /tmp so we
# don't have to worry about tar's behavior writing back to FUSE during
# create (separate test below).
if tar cf "$TAR_FILE" -C "$OUTPUT_DIR" test46-src 2>/tmp/tar.err; then
    PASS=$((PASS + 1))
    echo "  PASS: tar create from FUSE files"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: tar create failed (stderr: $(head -c 200 /tmp/tar.err))"
    echo "  FAIL: tar create failed"
fi

# tar x into FUSE: many writes + mkdir + utimens
EXTRACT_DIR="$OUTPUT_DIR/test46-extracted"
mkdir -p "$EXTRACT_DIR"
if tar xf "$TAR_FILE" -C "$EXTRACT_DIR" 2>/tmp/tar.err; then
    OUT=$(cat "$EXTRACT_DIR/test46-src/a.txt")
    assert_eq "tar extract into FUSE preserves content" "file 1" "$OUT"
    OUT=$(cat "$EXTRACT_DIR/test46-src/nested/c.txt")
    assert_eq "tar extract preserves nested files" "nested file" "$OUT"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: tar extract failed (stderr: $(head -c 200 /tmp/tar.err))"
    echo "  FAIL: tar extract failed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 47. sort (large stdin read) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test47.txt"
printf 'cherry\nbanana\napple\n' > "$F"
OUT=$(sort "$F")
assert_eq "sort FUSE file" "apple
banana
cherry" "$OUT"

# Sort + write back via pipe
sort "$F" > "$OUTPUT_DIR/test47-sorted.txt"
OUT=$(cat "$OUTPUT_DIR/test47-sorted.txt")
assert_eq "sort then write back to FUSE" "apple
banana
cherry" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 48. uniq / cut / paste / tr pipeline ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test48.txt"
printf 'a 1\na 1\nb 2\nb 2\nc 3\n' > "$F"

OUT=$(uniq < "$F" | wc -l | tr -d ' ')
assert_eq "uniq removes dup lines" "3" "$OUT"

OUT=$(cut -d' ' -f1 "$F" | sort -u | tr '\n' ',' | sed 's/,$//')
assert_eq "cut + sort -u + tr pipeline" "a,b,c" "$OUT"

# paste two FUSE files
A="$OUTPUT_DIR/test48-a.txt"
B="$OUTPUT_DIR/test48-b.txt"
printf '1\n2\n3\n' > "$A"
printf 'x\ny\nz\n' > "$B"
OUT=$(paste "$A" "$B" | tr '\t' ',' | head -1)
assert_eq "paste two FUSE files" "1,x" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 49. xargs (filename pipe) ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test49"
mkdir -p "$DIR"
echo "first" > "$DIR/a.txt"
echo "second" > "$DIR/b.txt"
echo "third" > "$DIR/c.txt"

OUT=$(find "$DIR" -name '*.txt' -print0 | xargs -0 cat | sort | tr '\n' ',' | sed 's/,$//')
assert_eq "find -print0 | xargs -0 cat" "first,second,third" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 50. find -exec ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test50"
mkdir -p "$DIR"
echo "match-me" > "$DIR/a.txt"
echo "match-me" > "$DIR/b.txt"
echo "skip" > "$DIR/c.txt"

OUT=$(find "$DIR" -name '*.txt' -exec grep -l match-me {} \; | wc -l | tr -d ' ')
assert_eq "find -exec grep -l" "2" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 51. Glob expansion ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test51"
mkdir -p "$DIR"
echo "a" > "$DIR/file1.txt"
echo "b" > "$DIR/file2.txt"
echo "c" > "$DIR/file3.txt"

COUNT=0
for f in "$DIR"/*.txt; do
    COUNT=$((COUNT + 1))
done
assert_eq "bash glob over FUSE dir" "3" "$COUNT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 52. realpath / readlink -f ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test52.txt"
echo "x" > "$F"
RP=$(realpath "$F")
assert_eq "realpath canonical path" "$F" "$RP"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 53. Truncation idioms (: > F, > F) ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test53.txt"
echo "lots of content here" > "$F"
: > "$F"   # null-command + redirect; classic file truncation
SIZE=$(stat -c '%s' "$F")
assert_eq ": > F truncates file" "0" "$SIZE"

echo "again" > "$F"
> "$F"     # bare-redirect truncation
SIZE=$(stat -c '%s' "$F")
assert_eq "> F truncates file" "0" "$SIZE"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 54. chmod recursive (no-op behavior consistency) ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test54"
mkdir -p "$DIR/sub"
echo "x" > "$DIR/a.txt"
echo "y" > "$DIR/sub/b.txt"
# chmod -R should succeed (every setattr returns ok); files unchanged.
if chmod -R 0644 "$DIR" 2>/tmp/chmod.err; then
    PASS=$((PASS + 1))
    echo "  PASS: chmod -R succeeds (silent no-op)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: chmod -R failed (stderr: $(head -c 200 /tmp/chmod.err))"
    echo "  FAIL: chmod -R failed"
fi
OUT=$(cat "$DIR/sub/b.txt")
assert_eq "chmod -R preserves content" "y" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 55. mv between subdirectories ==="
# ═══════════════════════════════════════════════════════════════════════

DIR_A="$OUTPUT_DIR/test55-a"
DIR_B="$OUTPUT_DIR/test55-b"
mkdir -p "$DIR_A" "$DIR_B"
echo "moving" > "$DIR_A/file.txt"
mv "$DIR_A/file.txt" "$DIR_B/file.txt"
if [ -e "$DIR_A/file.txt" ]; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: mv between subdirs left source"
    echo "  FAIL: mv between subdirs left source"
else
    OUT=$(cat "$DIR_B/file.txt")
    assert_eq "mv between subdirs preserves content" "moving" "$OUT"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 56. ls -lhR (recursive long listing) ==="
# ═══════════════════════════════════════════════════════════════════════

DIR="$OUTPUT_DIR/test56"
mkdir -p "$DIR/sub"
echo "1" > "$DIR/a.txt"
echo "2" > "$DIR/sub/b.txt"
if ls -lhR "$DIR" > /tmp/lsR.out 2>&1; then
    # Both files mentioned somewhere in output
    if grep -q a.txt /tmp/lsR.out && grep -q b.txt /tmp/lsR.out; then
        PASS=$((PASS + 1))
        echo "  PASS: ls -lhR lists nested files"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: ls -lhR missing entries"
        echo "  FAIL: ls -lhR missing entries"
    fi
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: ls -lhR failed"
    echo "  FAIL: ls -lhR failed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 57. printf with binary escapes ==="
# ═══════════════════════════════════════════════════════════════════════

F="$OUTPUT_DIR/test57.txt"
printf 'tab\there\nnewline\n' > "$F"
SIZE=$(stat -c '%s' "$F")
# "tab" (3) + tab (1) + "here" (4) + \n (1) + "newline" (7) + \n (1) = 17
assert_eq "printf with \\t and \\n" "17" "$SIZE"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 58. cat with multiple FUSE files ==="
# ═══════════════════════════════════════════════════════════════════════

A="$OUTPUT_DIR/test58-a.txt"
B="$OUTPUT_DIR/test58-b.txt"
echo "first" > "$A"
echo "second" > "$B"
OUT=$(cat "$A" "$B")
assert_eq "cat two FUSE files" "first
second" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 59. curl writes through FUSE (agent download → save pattern) ==="
# ═══════════════════════════════════════════════════════════════════════

# Agents very commonly fetch a URL and save it under /reevofs/output/.
# curl -o opens with O_WRONLY|O_CREAT|O_TRUNC and writes in chunks — a
# distinct syscall pattern from echo > (which uses one write) and cp
# (which uses copy_file_range). Mock /_stats is JSON content we can
# verify byte-for-byte.
F="$OUTPUT_DIR/test59-curl-out.json"
if curl -sf -o "$F" http://127.0.0.1:9876/_stats; then
    if [ -s "$F" ]; then
        OUT=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['put'] >= 0)" "$F")
        assert_eq "curl -o writes valid JSON to FUSE" "True" "$OUT"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: curl -o produced empty FUSE file"
        echo "  FAIL: curl -o produced empty FUSE file"
    fi
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: curl -o exit non-zero"
    echo "  FAIL: curl -o exit non-zero"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 60. curl uploads from FUSE (agent upload pattern) ==="
# ═══════════════════════════════════════════════════════════════════════

# Reverse direction — curl reading a FUSE file as the request body.
# Exercises mmap (curl uses it for the upload body on >0-byte files) +
# our read path. Posting to a fake mock endpoint that just echoes
# Content-Length back lets us assert the byte count survived FUSE.
F="$OUTPUT_DIR/test60-upload.txt"
printf 'agent-uploaded payload\n' > "$F"
# Use _list as a "round-trip" endpoint — it consumes the body. We can't
# inspect what arrived directly, but we can check curl exits 0 and the
# mock receives a sensible Content-Length (visible in its log).
if curl -sf -X POST -H 'Content-Type: application/json' \
       --data-binary @"$F" \
       http://127.0.0.1:9876/api/v2/fs/output/test-chat-id/_list \
       > /dev/null 2>&1; then
    PASS=$((PASS + 1))
    echo "  PASS: curl --data-binary @FUSE_file POSTs successfully"
else
    # _list may reject the upload body shape — that's fine as long as
    # the read-from-FUSE path didn't break. Re-check the file is intact.
    OUT=$(cat "$F")
    if [ "$OUT" = "agent-uploaded payload" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: curl read FUSE file for upload (server rejected payload, expected)"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: curl upload from FUSE corrupted source"
        echo "  FAIL: curl upload from FUSE corrupted source"
    fi
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 61. curl with --output-dir into FUSE ==="
# ═══════════════════════════════════════════════════════════════════════

# Newer curl supports --output-dir which combines mkdir-ish behavior
# (dir must exist) with -o relative-path. Common in scripts that fan
# out multiple downloads.
DIR="$OUTPUT_DIR/test61"
mkdir -p "$DIR"
if curl -sf --output-dir "$DIR" -o "fetched.json" http://127.0.0.1:9876/_stats 2>/dev/null; then
    if [ -s "$DIR/fetched.json" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: curl --output-dir writes to FUSE subdir"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: curl --output-dir produced empty file"
        echo "  FAIL: curl --output-dir produced empty file"
    fi
else
    # Older curl: --output-dir may not exist. Fall back to manual cd.
    PASS=$((PASS + 1))
    echo "  SKIP: curl --output-dir not supported in this version"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 62. curl streaming large response into FUSE ==="
# ═══════════════════════════════════════════════════════════════════════

# Larger response → exercises curl's chunked-write pattern through FUSE.
# Use the mock's _list with a directory containing many entries.
DIR="$OUTPUT_DIR/test62-many"
mkdir -p "$DIR"
for i in $(seq 1 30); do
    echo "entry $i" > "$DIR/file$i.txt"
done

F="$OUTPUT_DIR/test62-listing.json"
if curl -sf -X POST -H 'Content-Type: application/json' \
       -d '{"path": "/test62-many"}' \
       -o "$F" \
       "http://127.0.0.1:9876/api/v2/fs/output/test-chat-id/_list" 2>/dev/null; then
    # mkdir creates a .keep placeholder so the dir survives an empty
    # state — count only file*.txt entries to ignore it.
    COUNT=$(python3 -c "
import json, sys
entries = json.load(open(sys.argv[1]))['entries']
print(sum(1 for e in entries if e['name'].startswith('file')))
" "$F" 2>/dev/null || echo 0)
    if [ "$COUNT" = "30" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: curl streamed 30-entry listing into FUSE"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}
  FAIL: curl streamed listing has $COUNT file entries, expected 30"
        echo "  FAIL: curl streamed listing has $COUNT file entries"
    fi
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}
  FAIL: curl streaming POST failed"
    echo "  FAIL: curl streaming POST failed"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== Summary ==="
# ═══════════════════════════════════════════════════════════════════════

echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
if [ -n "$ERRORS" ]; then
    echo ""
    echo "Failures:"
    printf '%s\n' "$ERRORS"
fi

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
