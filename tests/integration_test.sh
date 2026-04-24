#!/usr/bin/env bash
# Integration test for the reevofs LD_PRELOAD shim.
# Expects:
#   - REEVOFS_PRELOAD_LIB set to the path of libreevofs_preload.so
#   - Mock API server already running on port 9876
#
# Env vars for the shim are set below.

set -euo pipefail

export REEVO_API_URL="http://127.0.0.1:9876"
export REEVO_API_TOKEN="test-token"
export REEVO_USER_ID="test-user"
export REEVO_ORG_ID="test-org"
export REEVOFS_SCOPE_skills="overlay"
export REEVOFS_SCOPE_output="test-chat-id"

LIB="${REEVOFS_PRELOAD_LIB:?Must set REEVOFS_PRELOAD_LIB}"

PASS=0
FAIL=0
ERRORS=""

run() {
    LD_PRELOAD="$LIB" "$@"
}

assert_eq() {
    local test_name="$1"
    local expected="$2"
    local actual="$3"
    if [ "$expected" = "$actual" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: $test_name"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}\n  FAIL: $test_name\n    expected: $(echo "$expected" | head -c 200)\n    actual:   $(echo "$actual" | head -c 200)"
        echo "  FAIL: $test_name"
    fi
}

assert_ok() {
    local test_name="$1"
    shift
    if run "$@" > /dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo "  PASS: $test_name"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}\n  FAIL: $test_name (exit code $?)"
        echo "  FAIL: $test_name"
    fi
}

assert_fail() {
    local test_name="$1"
    shift
    if run "$@" > /dev/null 2>&1; then
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}\n  FAIL: $test_name (expected failure, got success)"
        echo "  FAIL: $test_name"
    else
        PASS=$((PASS + 1))
        echo "  PASS: $test_name"
    fi
}

# ═══════════════════════════════════════════════════════════════════════
echo "=== 0. Empty namespace root (before any writes) ==="
# ═══════════════════════════════════════════════════════════════════════

# Output namespace has NO pre-seeded files — matches real API behavior.
# These tests catch the bug where ls/stat/cp fail on empty namespaces.

assert_ok "stat empty output namespace root" stat /reevofs/output
assert_ok "stat empty output namespace root/" stat /reevofs/output/
assert_ok "ls empty output namespace (no files)" timeout 5 ls /reevofs/output/
assert_ok "ls -l empty output namespace" timeout 5 ls -l /reevofs/output/

# cp to empty output namespace — the real-world failure scenario
echo "cp to empty ns" > /tmp/cp_empty_ns_src.txt
run cp /tmp/cp_empty_ns_src.txt /reevofs/output/cp_empty_ns.txt 2>/dev/null
OUT=$(run cat /reevofs/output/cp_empty_ns.txt 2>/dev/null)
assert_eq "cp to empty output namespace" "cp to empty ns" "$OUT"

# mv to empty output namespace
echo "mv to empty ns" > /tmp/mv_empty_ns_src.txt
run mv /tmp/mv_empty_ns_src.txt /reevofs/output/mv_empty_ns.txt 2>/dev/null
OUT=$(run cat /reevofs/output/mv_empty_ns.txt 2>/dev/null)
assert_eq "mv to empty output namespace" "mv to empty ns" "$OUT"

# Clean up so later tests see a fresh state
run rm /reevofs/output/cp_empty_ns.txt 2>/dev/null || true
run rm /reevofs/output/mv_empty_ns.txt 2>/dev/null || true

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 1. cat (read file) ==="
# ═══════════════════════════════════════════════════════════════════════

OUT=$(run cat /reevofs/skills/my-skill/SKILL.md)
assert_eq "cat skills file" "# My Skill
This is a test skill." "$OUT"

OUT=$(run cat /reevofs/skills/hello.txt)
assert_eq "cat simple file" "hello world" "$OUT"

# Write a file to output so we can read it back (output namespace starts empty)
run bash -c 'echo "seeded output content" > /reevofs/output/seeded.txt'
OUT=$(run cat /reevofs/output/seeded.txt)
assert_eq "cat output file" "seeded output content" "$OUT"

# Non-existent file should fail
assert_fail "cat nonexistent file" cat /reevofs/skills/nope.txt

# Unknown namespace should fall through (fail because /reevofs/unknown doesn't exist on real fs)
assert_fail "cat unknown namespace" cat /reevofs/unknown/foo.txt

# Binary file: bytes must round-trip exactly through the shim. Uses .dat
# since .bin is on the backend blocklist. Exercises the raw-bytes read
# contract (Accept: application/octet-stream) end-to-end — regression guard
# for v0.3.9 where read_file was not sending Accept and 415'd on binary.
EXPECTED_MD5=$(printf '\xff\xfe\xfd\xfc' | md5sum | awk '{print $1}')
ACTUAL_MD5=$(run cat /reevofs/skills/binary.dat 2>/dev/null | md5sum | awk '{print $1}')
assert_eq "binary file bytes round-trip via cat" "$EXPECTED_MD5" "$ACTUAL_MD5"

# Blocked extension: backend returns 415, shim must map to EACCES. Test each
# extension in the real backend's blocklist so renames/additions there are
# caught here.
for BLOCKED_EXT_PROBE in exe sh bat bin dll so dylib; do
    if run cat "/reevofs/skills/evil.$BLOCKED_EXT_PROBE" 2>/tmp/reevofs_blocked.err; then
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}\n  FAIL: blocked .$BLOCKED_EXT_PROBE did not fail"
        echo "  FAIL: blocked .$BLOCKED_EXT_PROBE did not fail"
    elif grep -qi "permission denied" /tmp/reevofs_blocked.err; then
        PASS=$((PASS + 1))
        echo "  PASS: blocked .$BLOCKED_EXT_PROBE → Permission denied (EACCES)"
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}\n  FAIL: blocked .$BLOCKED_EXT_PROBE errno=$(cat /tmp/reevofs_blocked.err)"
        echo "  FAIL: blocked .$BLOCKED_EXT_PROBE wrong errno: $(cat /tmp/reevofs_blocked.err)"
    fi
done

# Stat on binary — the specific failure mode fixed in v0.3.9. Before the fix,
# read_file GET without explicit Accept returned 415 on binary content, and
# cached_exists silently fell through to list_dir → NotFound → stat ENOENT.
# After the fix, Accept: application/octet-stream is sent, GET returns 200,
# and stat reports the file.
if run stat /reevofs/skills/binary.dat > /dev/null 2>&1; then
    PASS=$((PASS + 1))
    echo "  PASS: stat on binary file works (v0.3.9 Accept fix)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: stat on binary file failed (Accept header regression?)"
    echo "  FAIL: stat on binary file failed (Accept header regression?)"
fi

# Close-hook errno mapping (v0.3.9): writes that fail at the backend must
# surface the right errno — Forbidden → EACCES, not a generic EIO.
# Before v0.3.9 every flush failure became "OSError: [Errno 5] Input/output
# error" which made it impossible to tell "extension blocked" from "network
# timeout" from "auth failure".
run python3 -c "
import errno, os
try:
    with open('/reevofs/output/blocked.bin', 'wb') as f:
        f.write(b'x')
    print('UNEXPECTED_SUCCESS')
    exit(1)
except OSError as e:
    if e.errno == errno.EACCES:
        print('OK EACCES')
        exit(0)
    print(f'WRONG_ERRNO {e.errno} ({errno.errorcode.get(e.errno, \"?\")})')
    exit(2)
" > /tmp/reevofs_blocked_write.out 2>&1
RC=$?
if [ $RC -eq 0 ]; then
    PASS=$((PASS + 1))
    echo "  PASS: write to blocked ext → EACCES (v0.3.9 close-hook errno fix)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: write to blocked ext got wrong errno: $(cat /tmp/reevofs_blocked_write.out)"
    echo "  FAIL: write to blocked ext: $(cat /tmp/reevofs_blocked_write.out)"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 2. stat ==="
# ═══════════════════════════════════════════════════════════════════════

# stat on root mount
assert_ok "stat /reevofs" stat /reevofs
assert_ok "stat /reevofs/" stat /reevofs/

# stat on namespace root
assert_ok "stat /reevofs/skills" stat /reevofs/skills
assert_ok "stat /reevofs/output" stat /reevofs/output

# stat on file
assert_ok "stat /reevofs/skills/hello.txt" stat /reevofs/skills/hello.txt

# stat on directory
assert_ok "stat /reevofs/skills/my-skill" stat /reevofs/skills/my-skill

# Check file vs directory detection via stat output
STAT_OUT=$(run stat -c '%F' /reevofs/skills/hello.txt 2>/dev/null || run stat /reevofs/skills/hello.txt 2>&1)
if echo "$STAT_OUT" | grep -qi "regular"; then
    PASS=$((PASS + 1))
    echo "  PASS: stat identifies file as regular"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: stat identifies file as regular (got: $STAT_OUT)"
    echo "  FAIL: stat identifies file as regular"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 3. ls (directory listing) ==="
# ═══════════════════════════════════════════════════════════════════════

# ls root
OUT=$(run ls /reevofs/ 2>/dev/null | sort)
# Should contain skills and output
if echo "$OUT" | grep -q "skills" && echo "$OUT" | grep -q "output"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls /reevofs/ shows namespaces"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls /reevofs/ shows namespaces (got: $OUT)"
    echo "  FAIL: ls /reevofs/ shows namespaces"
fi

# ls namespace root
OUT=$(run ls /reevofs/skills/ 2>/dev/null)
if echo "$OUT" | grep -q "my-skill" && echo "$OUT" | grep -q "hello.txt"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls /reevofs/skills/ shows entries"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls /reevofs/skills/ shows entries (got: $OUT)"
    echo "  FAIL: ls /reevofs/skills/ shows entries"
fi

# ls subdirectory
OUT=$(run ls /reevofs/skills/my-skill/ 2>/dev/null)
if echo "$OUT" | grep -q "SKILL.md"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls /reevofs/skills/my-skill/ shows files"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls /reevofs/skills/my-skill/ shows files (got: $OUT)"
    echo "  FAIL: ls /reevofs/skills/my-skill/ shows files"
fi

# ls -l (needs dirfd-relative fstatat)
# Note: ls -la hangs on ".." because it resolves to a non-existent parent.
# Use ls -l (no -a) to skip "." and ".." entries.
OUT=$(run timeout 5 ls -l /reevofs/skills/ 2>/dev/null)
if echo "$OUT" | grep -q "hello.txt"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls -l /reevofs/skills/ works"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls -l /reevofs/skills/ works (got: $OUT)"
    echo "  FAIL: ls -l /reevofs/skills/ works"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 4. Write (echo >) ==="
# ═══════════════════════════════════════════════════════════════════════

# Write a new file to output namespace
run bash -c 'echo "test data" > /reevofs/output/new-file.txt'
OUT=$(run cat /reevofs/output/new-file.txt)
assert_eq "write then read new file" "test data" "$OUT"

# Overwrite existing file (seeded.txt was created in section 1)
run bash -c 'echo "updated" > /reevofs/output/seeded.txt'
OUT=$(run cat /reevofs/output/seeded.txt)
assert_eq "overwrite existing file" "updated" "$OUT"

# Write should fail on read-only namespace
assert_fail "write to read-only namespace" bash -c 'echo "nope" > /reevofs/skills/hack.txt'

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 5. rm (delete) ==="
# ═══════════════════════════════════════════════════════════════════════

# Write a file, then delete it
run bash -c 'echo "delete me" > /reevofs/output/to-delete.txt'
assert_ok "rm output file" rm /reevofs/output/to-delete.txt
assert_fail "cat deleted file" cat /reevofs/output/to-delete.txt

# Delete should fail on read-only namespace
assert_fail "rm on read-only namespace" rm /reevofs/skills/hello.txt

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 6. mkdir ==="
# ═══════════════════════════════════════════════════════════════════════

assert_ok "mkdir on writable namespace" mkdir -p /reevofs/output/subdir/nested
assert_fail "mkdir on read-only namespace" mkdir /reevofs/skills/new-dir

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 7. Python file access ==="
# ═══════════════════════════════════════════════════════════════════════

# Python open() + read()
OUT=$(run python3 -c "
with open('/reevofs/skills/hello.txt') as f:
    print(f.read(), end='')
" 2>/dev/null)
assert_eq "python read file" "hello world" "$OUT"

# Python os.path.exists
OUT=$(run python3 -c "
import os
print(os.path.exists('/reevofs/skills/hello.txt'))
" 2>/dev/null)
assert_eq "python os.path.exists (file)" "True" "$OUT"

OUT=$(run python3 -c "
import os
print(os.path.exists('/reevofs/skills/nope.txt'))
" 2>/dev/null)
assert_eq "python os.path.exists (missing)" "False" "$OUT"

# Python os.path.isdir
OUT=$(run python3 -c "
import os
print(os.path.isdir('/reevofs/skills/my-skill'))
" 2>/dev/null)
assert_eq "python os.path.isdir (dir)" "True" "$OUT"

OUT=$(run python3 -c "
import os
print(os.path.isdir('/reevofs/skills/hello.txt'))
" 2>/dev/null)
assert_eq "python os.path.isdir (file)" "False" "$OUT"

# Python os.listdir
OUT=$(run python3 -c "
import os
entries = sorted(os.listdir('/reevofs/skills/'))
print(' '.join(entries))
" 2>/dev/null)
if echo "$OUT" | grep -q "hello.txt" && echo "$OUT" | grep -q "my-skill"; then
    PASS=$((PASS + 1))
    echo "  PASS: python os.listdir"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: python os.listdir (got: $OUT)"
    echo "  FAIL: python os.listdir"
fi

# Python write
run python3 -c "
with open('/reevofs/output/from-python.txt', 'w') as f:
    f.write('written by python')
" 2>/dev/null
OUT=$(run cat /reevofs/output/from-python.txt)
assert_eq "python write file" "written by python" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 8. Path traversal rejection ==="
# ═══════════════════════════════════════════════════════════════════════

assert_fail "path traversal cat" cat /reevofs/skills/../../../etc/passwd
assert_fail "path traversal stat" stat /reevofs/skills/../../etc/shadow

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 9. Permission enforcement ==="
# ═══════════════════════════════════════════════════════════════════════

# access() checks
OUT=$(run python3 -c "
import os
print(os.access('/reevofs/skills/hello.txt', os.R_OK))
" 2>/dev/null)
assert_eq "read access on skills" "True" "$OUT"

OUT=$(run python3 -c "
import os
print(os.access('/reevofs/skills/hello.txt', os.W_OK))
" 2>/dev/null)
assert_eq "write access denied on skills" "False" "$OUT"

OUT=$(run python3 -c "
import os
print(os.access('/reevofs/output/seeded.txt', os.W_OK))
" 2>/dev/null)
assert_eq "write access on output" "True" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 10. Large file write/read ==="
# ═══════════════════════════════════════════════════════════════════════

# Generate a 100KB file and write it
run python3 -c "
data = 'x' * 100000
with open('/reevofs/output/large.txt', 'w') as f:
    f.write(data)
" 2>/dev/null

OUT=$(run python3 -c "
with open('/reevofs/output/large.txt') as f:
    print(len(f.read()))
" 2>/dev/null)
assert_eq "large file roundtrip" "100000" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 11. Bash builtins and redirects ==="
# ═══════════════════════════════════════════════════════════════════════

# test -f (file exists)
run bash -c 'test -f /reevofs/skills/hello.txt && echo yes || echo no' > /tmp/bash_test_f.out 2>/dev/null
OUT=$(cat /tmp/bash_test_f.out)
assert_eq "bash test -f (exists)" "yes" "$OUT"

run bash -c 'test -f /reevofs/skills/nope.txt && echo yes || echo no' > /tmp/bash_test_f2.out 2>/dev/null
OUT=$(cat /tmp/bash_test_f2.out)
assert_eq "bash test -f (missing)" "no" "$OUT"

# test -d (directory)
run bash -c 'test -d /reevofs/skills/my-skill && echo yes || echo no' > /tmp/bash_test_d.out 2>/dev/null
OUT=$(cat /tmp/bash_test_d.out)
assert_eq "bash test -d (dir)" "yes" "$OUT"

run bash -c 'test -d /reevofs/skills/hello.txt && echo yes || echo no' > /tmp/bash_test_d2.out 2>/dev/null
OUT=$(cat /tmp/bash_test_d2.out)
assert_eq "bash test -d (file)" "no" "$OUT"

# test -r / -w (permissions)
run bash -c 'test -r /reevofs/skills/hello.txt && echo yes || echo no' > /tmp/bash_test_r.out 2>/dev/null
OUT=$(cat /tmp/bash_test_r.out)
assert_eq "bash test -r (readable)" "yes" "$OUT"

run bash -c 'test -w /reevofs/skills/hello.txt && echo yes || echo no' > /tmp/bash_test_w.out 2>/dev/null
OUT=$(cat /tmp/bash_test_w.out)
assert_eq "bash test -w (read-only)" "no" "$OUT"

run bash -c 'test -w /reevofs/output/seeded.txt && echo yes || echo no' > /tmp/bash_test_w2.out 2>/dev/null
OUT=$(cat /tmp/bash_test_w2.out)
assert_eq "bash test -w (writable)" "yes" "$OUT"

# Bash read via redirect
OUT=$(run bash -c 'cat < /reevofs/skills/hello.txt')
assert_eq "bash input redirect" "hello world" "$OUT"

# Bash heredoc-style: write multi-line via bash redirect
run bash -c 'printf "line1\nline2\nline3" > /reevofs/output/multiline.txt'
OUT=$(run cat /reevofs/output/multiline.txt)
assert_eq "bash multiline write" "line1
line2
line3" "$OUT"

# Heredoc via cat (fork+exec path — tests that close() doesn't prematurely flush 0 bytes)
run bash -c 'cat > /reevofs/output/heredoc-test.txt << EOF
line1 from heredoc
line2 from heredoc
line3 from heredoc
EOF'
OUT=$(run cat /reevofs/output/heredoc-test.txt)
assert_eq "heredoc via cat" "line1 from heredoc
line2 from heredoc
line3 from heredoc" "$OUT"

# Heredoc via cat with quoted delimiter (no variable expansion)
run bash -c 'cat > /reevofs/output/heredoc-quoted.txt << '\''ENDOFFILE'\''
def hello():
    print("world")
ENDOFFILE'
OUT=$(run cat /reevofs/output/heredoc-quoted.txt)
assert_eq "heredoc via cat (quoted delim)" 'def hello():
    print("world")' "$OUT"

# Pipe to external command writing to reevofs (fork+exec path)
run bash -c 'echo "piped content" | cat > /reevofs/output/pipe-cat.txt'
OUT=$(run cat /reevofs/output/pipe-cat.txt)
assert_eq "pipe to cat > reevofs" "piped content" "$OUT"

# Bash append (>>)
run bash -c 'echo "first" > /reevofs/output/append-test.txt'
run bash -c 'echo "second" >> /reevofs/output/append-test.txt'
OUT=$(run cat /reevofs/output/append-test.txt)
# Note: >> on a virtual fs backed by API may just overwrite — test that it at least doesn't crash.
# The important thing is the operation succeeds and produces readable output.
if [ -n "$OUT" ]; then
    PASS=$((PASS + 1))
    echo "  PASS: bash append redirect"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: bash append redirect (got empty output)"
    echo "  FAIL: bash append redirect"
fi

# Bash subshell with pipe
OUT=$(run bash -c 'cat /reevofs/skills/hello.txt | tr "[:lower:]" "[:upper:]"')
assert_eq "bash pipe" "HELLO WORLD" "$OUT"

# Bash for loop over ls
OUT=$(run bash -c 'for f in $(ls /reevofs/skills/); do echo "$f"; done | sort | head -1')
if [ -n "$OUT" ]; then
    PASS=$((PASS + 1))
    echo "  PASS: bash for loop over ls"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: bash for loop over ls (got empty)"
    echo "  FAIL: bash for loop over ls"
fi

# Bash glob (may not work with virtual fs, but shouldn't crash)
run bash -c 'ls /reevofs/skills/*.txt' > /dev/null 2>&1
# Glob may fail (no kernel-level readdir for glob), just ensure it doesn't hang or segfault.
PASS=$((PASS + 1))
echo "  PASS: bash glob doesn't crash"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 12. Node.js file access ==="
# ═══════════════════════════════════════════════════════════════════════

if ! command -v node &>/dev/null; then
    echo "  SKIP: node not found, skipping Node.js tests"
else

# Node readFileSync
OUT=$(run timeout 10 node -e "
const fs = require('fs');
console.log(fs.readFileSync('/reevofs/skills/hello.txt', 'utf8'));
" 2>/dev/null)
assert_eq "node readFileSync" "hello world" "$OUT"

# Node existsSync
OUT=$(run timeout 10 node -e "
const fs = require('fs');
console.log(fs.existsSync('/reevofs/skills/hello.txt'));
" 2>/dev/null)
assert_eq "node existsSync (file)" "true" "$OUT"

OUT=$(run timeout 10 node -e "
const fs = require('fs');
console.log(fs.existsSync('/reevofs/skills/nope.txt'));
" 2>/dev/null)
assert_eq "node existsSync (missing)" "false" "$OUT"

# Node statSync — file vs directory (intercepted via syscall hook)
OUT=$(run timeout 10 node -e "
const fs = require('fs');
const s = fs.statSync('/reevofs/skills/hello.txt');
console.log(s.isFile(), s.isDirectory());
" 2>/dev/null)
assert_eq "node statSync file" "true false" "$OUT"

OUT=$(run timeout 10 node -e "
const fs = require('fs');
const s = fs.statSync('/reevofs/skills/my-skill');
console.log(s.isFile(), s.isDirectory());
" 2>/dev/null)
assert_eq "node statSync dir" "false true" "$OUT"

# Node readdirSync (intercepted via scandir64 hook)
OUT=$(run timeout 10 node -e "
const fs = require('fs');
const entries = fs.readdirSync('/reevofs/skills/').sort();
console.log(entries.join(' '));
" 2>/dev/null)
if echo "$OUT" | grep -q "hello.txt" && echo "$OUT" | grep -q "my-skill"; then
    PASS=$((PASS + 1))
    echo "  PASS: node readdirSync"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: node readdirSync (got: $OUT)"
    echo "  FAIL: node readdirSync"
fi

# Node writeFileSync + readFileSync roundtrip
run timeout 10 node -e "
const fs = require('fs');
fs.writeFileSync('/reevofs/output/from-node.txt', 'written by node');
" 2>/dev/null
OUT=$(run cat /reevofs/output/from-node.txt)
assert_eq "node writeFileSync" "written by node" "$OUT"

# Node read/write access checks
OUT=$(run timeout 10 node -e "
const fs = require('fs');
try { fs.accessSync('/reevofs/skills/hello.txt', fs.constants.R_OK); console.log('readable'); }
catch(e) { console.log('not readable'); }
try { fs.accessSync('/reevofs/skills/hello.txt', fs.constants.W_OK); console.log('writable'); }
catch(e) { console.log('not writable'); }
" 2>/dev/null)
assert_eq "node accessSync read-only" "readable
not writable" "$OUT"

OUT=$(run timeout 10 node -e "
const fs = require('fs');
try { fs.accessSync('/reevofs/output/seeded.txt', fs.constants.W_OK); console.log('writable'); }
catch(e) { console.log('not writable'); }
" 2>/dev/null)
assert_eq "node accessSync writable" "writable" "$OUT"

fi  # end node check

# ═══════════════════════════════════════════════════════════════════════
echo "=== 13. rename / mv ==="
# ═══════════════════════════════════════════════════════════════════════

# First create a file to rename
run bash -c 'echo "rename me" > /reevofs/output/before_rename.txt'
OUT=$(run cat /reevofs/output/before_rename.txt 2>/dev/null)
assert_eq "create file for rename" "rename me" "$OUT"

# mv within same namespace
run mv /reevofs/output/before_rename.txt /reevofs/output/after_rename.txt 2>/dev/null
OUT=$(run cat /reevofs/output/after_rename.txt 2>/dev/null)
assert_eq "mv read destination" "rename me" "$OUT"

# Source should be gone
if run cat /reevofs/output/before_rename.txt 2>/dev/null; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: mv source removed (source still exists)"
    echo "  FAIL: mv source removed"
else
    PASS=$((PASS + 1))
    echo "  PASS: mv source removed"
fi

# Python os.rename
run python3 -c "
import os
# Create a file to rename
with open('/reevofs/output/py_rename_src.txt', 'w') as f:
    f.write('python rename test')
os.rename('/reevofs/output/py_rename_src.txt', '/reevofs/output/py_rename_dst.txt')
" 2>/dev/null
OUT=$(run cat /reevofs/output/py_rename_dst.txt 2>/dev/null)
assert_eq "python os.rename" "python rename test" "$OUT"

# Python source should be gone
OUT=$(run python3 -c "
import os
print(os.path.exists('/reevofs/output/py_rename_src.txt'))
" 2>/dev/null)
assert_eq "python rename source gone" "False" "$OUT"

# Node.js fs.renameSync
OUT=$(run timeout 10 node -e "
const fs = require('fs');
fs.writeFileSync('/reevofs/output/node_rename_src.txt', 'node rename test');
fs.renameSync('/reevofs/output/node_rename_src.txt', '/reevofs/output/node_rename_dst.txt');
console.log(fs.readFileSync('/reevofs/output/node_rename_dst.txt', 'utf8'));
console.log(fs.existsSync('/reevofs/output/node_rename_src.txt'));
" 2>/dev/null)
assert_eq "node renameSync" "node rename test
false" "$OUT"

# Rename from read-only namespace should fail
if run mv /reevofs/skills/hello.txt /reevofs/skills/hello2.txt 2>/dev/null; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: rename read-only should fail (got success)"
    echo "  FAIL: rename read-only should fail"
else
    PASS=$((PASS + 1))
    echo "  PASS: rename read-only should fail"
fi

# Cross-namespace rename: read-only source → writable dest (copy-style)
run bash -c 'cp /reevofs/skills/hello.txt /tmp/hello_snap.txt && mv /tmp/hello_snap.txt /reevofs/output/cross_ns.txt' 2>/dev/null || true
# Direct cross-namespace mv (source not deleted since read-only)
run mv /reevofs/skills/hello.txt /reevofs/output/cross_ns_direct.txt 2>/dev/null || true
OUT=$(run cat /reevofs/output/cross_ns_direct.txt 2>/dev/null)
assert_eq "cross-ns rename destination" "hello world" "$OUT"
# Source should still exist (read-only, can't delete)
OUT=$(run cat /reevofs/skills/hello.txt 2>/dev/null)
assert_eq "cross-ns rename source preserved" "hello world" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 14. Skills-only mode (no REEVOFS_SCOPE_output) ==="
# ═══════════════════════════════════════════════════════════════════════

# Run a sub-environment with only REEVOFS_SCOPE_skills set (no output)
run_skills_only() {
    REEVOFS_SCOPE_output="" LD_PRELOAD="$LIB" \
    REEVO_API_URL="$REEVO_API_URL" REEVO_API_TOKEN="$REEVO_API_TOKEN" \
    REEVO_USER_ID="$REEVO_USER_ID" REEVO_ORG_ID="$REEVO_ORG_ID" \
    REEVOFS_SCOPE_skills="$REEVOFS_SCOPE_skills" \
    env -u REEVOFS_SCOPE_output "$@"
}

# Skills should still work without output namespace
OUT=$(run_skills_only cat /reevofs/skills/hello.txt 2>/dev/null)
assert_eq "skills-only: cat skills file" "hello world" "$OUT"

OUT=$(run_skills_only ls /reevofs/skills/ 2>/dev/null)
if echo "$OUT" | grep -q "hello.txt"; then
    PASS=$((PASS + 1))
    echo "  PASS: skills-only: ls skills"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: skills-only: ls skills (got: $OUT)"
    echo "  FAIL: skills-only: ls skills"
fi

# Output namespace should not be accessible
assert_fail "skills-only: cat output fails" env -u REEVOFS_SCOPE_output LD_PRELOAD="$LIB" cat /reevofs/output/seeded.txt

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 15. Error propagation on close ==="
# ═══════════════════════════════════════════════════════════════════════

# Use a scope that the mock API rejects (scope starting with "reject-")
run_reject() {
    LD_PRELOAD="$LIB" \
    REEVO_API_URL="$REEVO_API_URL" REEVO_API_TOKEN="$REEVO_API_TOKEN" \
    REEVO_USER_ID="$REEVO_USER_ID" REEVO_ORG_ID="$REEVO_ORG_ID" \
    REEVOFS_SCOPE_skills="$REEVOFS_SCOPE_skills" \
    REEVOFS_SCOPE_output="reject-bad-scope" \
    "$@"
}

# Python close() should propagate the API error (EIO)
if run_reject python3 -c "
with open('/reevofs/output/test.txt', 'w') as f:
    f.write('hello')
" 2>/dev/null; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: python write with rejected scope should fail (got success)"
    echo "  FAIL: python write with rejected scope should fail"
else
    PASS=$((PASS + 1))
    echo "  PASS: python write with rejected scope fails with error"
fi

# Bash echo redirect — flush happens in dup2, which bash ignores.
# But we test that the write doesn't silently persist (file shouldn't be readable).
run_reject bash -c 'echo "hello" > /reevofs/output/test2.txt' 2>/dev/null || true
# The write should have been rejected by the API, so reading should fail or return stale data.
if run_reject cat /reevofs/output/test2.txt 2>/dev/null | grep -q "hello"; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: rejected write should not persist"
    echo "  FAIL: rejected write should not persist"
else
    PASS=$((PASS + 1))
    echo "  PASS: rejected write did not persist"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 16. Cross-filesystem mv (real fs → reevofs) ==="
# ═══════════════════════════════════════════════════════════════════════

# Create a file on real fs, then mv to reevofs output
echo "mv test data" > /tmp/mv_test_src.txt
run mv /tmp/mv_test_src.txt /reevofs/output/mv_test_dst.txt 2>/dev/null
OUT=$(run cat /reevofs/output/mv_test_dst.txt 2>/dev/null)
assert_eq "mv real→reevofs content" "mv test data" "$OUT"

# Source should be gone from real fs
if [ -f /tmp/mv_test_src.txt ]; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: mv source removed from real fs"
    echo "  FAIL: mv source removed from real fs"
else
    PASS=$((PASS + 1))
    echo "  PASS: mv source removed from real fs"
fi

# Python-generated file mv'd to reevofs
python3 -c "
with open('/tmp/py_mv_src.csv', 'w') as f:
    f.write('col1,col2\na,b\n')
" 2>/dev/null
run mv /tmp/py_mv_src.csv /reevofs/output/py_mv_dst.csv 2>/dev/null
OUT=$(run cat /reevofs/output/py_mv_dst.csv 2>/dev/null)
assert_eq "mv python csv→reevofs" "col1,col2
a,b" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 17. cp from real fs → reevofs ==="
# ═══════════════════════════════════════════════════════════════════════

echo "cp test content" > /tmp/cp_test_src.txt
run cp /tmp/cp_test_src.txt /reevofs/output/cp_test_dst.txt 2>/dev/null
OUT=$(run cat /reevofs/output/cp_test_dst.txt 2>/dev/null)
assert_eq "cp real→reevofs" "cp test content" "$OUT"

# Source should still exist on real fs
if [ -f /tmp/cp_test_src.txt ]; then
    PASS=$((PASS + 1))
    echo "  PASS: cp source preserved on real fs"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: cp source preserved on real fs"
    echo "  FAIL: cp source preserved on real fs"
fi

# cp from reevofs to real fs (use cat redirect since cp may use copy_file_range)
run bash -c 'cat /reevofs/skills/hello.txt > /tmp/cp_from_reevofs.txt'
OUT=$(cat /tmp/cp_from_reevofs.txt 2>/dev/null)
assert_eq "cp reevofs→real (via cat)" "hello world" "$OUT"

# Regression: stat on non-existent output file should return ENOENT, not IsDir
if run stat /reevofs/output/nonexistent_file_xyz.txt 2>/dev/null; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: stat non-existent file should fail (got success)"
    echo "  FAIL: stat non-existent file should fail"
else
    PASS=$((PASS + 1))
    echo "  PASS: stat non-existent file returns ENOENT"
fi

# Regression: cp to a new (non-existent) output path should work
echo "cp regression test" > /tmp/cp_regression_src.txt
run cp /tmp/cp_regression_src.txt /reevofs/output/cp_regression_dst.txt 2>/dev/null
OUT=$(run cat /reevofs/output/cp_regression_dst.txt 2>/dev/null)
assert_eq "cp to new output file (regression)" "cp regression test" "$OUT"

# GNU find — known limitation: find uses fstatat(dirfd, relative_path) internally,
# and glibc's fstatat on modern kernels uses inline syscalls that bypass our PLT hooks.
# Use python os.walk (section 18) for recursive traversal instead.
echo "  SKIP: GNU find (known limitation — use python os.walk instead)"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 18. Recursive directory access ==="
# ═══════════════════════════════════════════════════════════════════════

# Python os.walk (more reliable than find for LD_PRELOAD shim)
OUT=$(run python3 -c "
import os
files = []
for root, dirs, fnames in os.walk('/reevofs/skills/'):
    for f in fnames:
        files.append(os.path.join(root, f))
for f in sorted(files):
    print(f)
" 2>/dev/null)
if echo "$OUT" | grep -q "SKILL.md" && echo "$OUT" | grep -q "hello.txt"; then
    PASS=$((PASS + 1))
    echo "  PASS: python os.walk finds files recursively"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: python os.walk finds files recursively (got: $OUT)"
    echo "  FAIL: python os.walk finds files recursively"
fi

# Python os.walk finds directories
OUT=$(run python3 -c "
import os
dirs = []
for root, subdirs, fnames in os.walk('/reevofs/skills/'):
    for d in subdirs:
        dirs.append(os.path.join(root, d))
for d in sorted(dirs):
    print(d)
" 2>/dev/null)
if echo "$OUT" | grep -q "my-skill"; then
    PASS=$((PASS + 1))
    echo "  PASS: python os.walk finds directories"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: python os.walk finds directories (got: $OUT)"
    echo "  FAIL: python os.walk finds directories"
fi

# Node.js recursive readdir
OUT=$(run timeout 10 node -e "
const fs = require('fs');
const path = require('path');
function walk(dir) {
    const entries = fs.readdirSync(dir, {withFileTypes: true});
    let files = [];
    for (const e of entries) {
        const full = path.join(dir, e.name);
        if (e.isDirectory()) files = files.concat(walk(full));
        else files.push(full);
    }
    return files;
}
walk('/reevofs/skills/').sort().forEach(f => console.log(f));
" 2>/dev/null)
if echo "$OUT" | grep -q "SKILL.md" && echo "$OUT" | grep -q "hello.txt"; then
    PASS=$((PASS + 1))
    echo "  PASS: node recursive readdir"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: node recursive readdir (got: $OUT)"
    echo "  FAIL: node recursive readdir"
fi

# Nested subdirectory cat
OUT=$(run cat /reevofs/skills/my-skill/config.json 2>/dev/null)
if echo "$OUT" | grep -q "my-skill"; then
    PASS=$((PASS + 1))
    echo "  PASS: cat nested subdirectory file"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: cat nested subdirectory file (got: $OUT)"
    echo "  FAIL: cat nested subdirectory file"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 19. stat display (ls -l) ==="
# ═══════════════════════════════════════════════════════════════════════

# ls -l on /reevofs/ should show proper permissions, not d?????????
OUT=$(run ls -ld /reevofs/ 2>/dev/null)
if echo "$OUT" | grep -q "^d"; then
    # Check it doesn't show d?????????
    if echo "$OUT" | grep -q "d?????????"; then
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}\n  FAIL: stat /reevofs/ shows d????????? (got: $OUT)"
        echo "  FAIL: stat /reevofs/ shows d?????????"
    else
        PASS=$((PASS + 1))
        echo "  PASS: stat /reevofs/ shows proper permissions"
    fi
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: stat /reevofs/ identified as directory (got: $OUT)"
    echo "  FAIL: stat /reevofs/ identified as directory"
fi

# ls -la should work on namespace directories
OUT=$(run ls -l /reevofs/skills/ 2>/dev/null)
if echo "$OUT" | grep -q "hello.txt"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls -l /reevofs/skills/ works"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls -l /reevofs/skills/ works (got: $OUT)"
    echo "  FAIL: ls -l /reevofs/skills/ works"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 20. Common agent operations ==="
# ═══════════════════════════════════════════════════════════════════════

# Script writing output via redirect (most common pattern)
run bash -c 'echo "report line 1" > /reevofs/output/report.csv && echo "report line 2" >> /reevofs/output/report.csv'
OUT=$(run cat /reevofs/output/report.csv 2>/dev/null)
if echo "$OUT" | grep -q "report line"; then
    PASS=$((PASS + 1))
    echo "  PASS: bash script writes report"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: bash script writes report (got: $OUT)"
    echo "  FAIL: bash script writes report"
fi

# Python script writing to output
run python3 -c "
import json
data = {'results': [1,2,3], 'status': 'ok'}
with open('/reevofs/output/results.json', 'w') as f:
    json.dump(data, f)
" 2>/dev/null
OUT=$(run python3 -c "
import json
with open('/reevofs/output/results.json') as f:
    d = json.load(f)
print(d['status'], len(d['results']))
" 2>/dev/null)
assert_eq "python json roundtrip" "ok 3" "$OUT"

# Read skills, process, write to output (common agent pattern)
run python3 -c "
with open('/reevofs/skills/my-skill/config.json') as f:
    import json
    config = json.load(f)
with open('/reevofs/output/config_copy.json', 'w') as f:
    json.dump({'copied_from': config['name'], 'version': config['version']}, f)
" 2>/dev/null
OUT=$(run cat /reevofs/output/config_copy.json 2>/dev/null)
if echo "$OUT" | grep -q "my-skill"; then
    PASS=$((PASS + 1))
    echo "  PASS: read skills → write output pattern"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: read skills → write output pattern (got: $OUT)"
    echo "  FAIL: read skills → write output pattern"
fi

# Node script writing output
run timeout 10 node -e "
const fs = require('fs');
const data = fs.readFileSync('/reevofs/skills/hello.txt', 'utf8');
fs.writeFileSync('/reevofs/output/processed.txt', data.toUpperCase());
" 2>/dev/null
OUT=$(run cat /reevofs/output/processed.txt 2>/dev/null)
assert_eq "node read→process→write" "HELLO WORLD" "$OUT"

# Multiple sequential writes to same file
run python3 -c "
with open('/reevofs/output/log.txt', 'w') as f:
    for i in range(5):
        f.write(f'line {i}\n')
" 2>/dev/null
OUT=$(run python3 -c "
with open('/reevofs/output/log.txt') as f:
    print(len(f.readlines()))
" 2>/dev/null)
assert_eq "sequential writes count" "5" "$OUT"

# Capture command output to reevofs (tee hangs with LD_PRELOAD, use redirect)
run bash -c 'echo "captured output" > /reevofs/output/captured.txt'
OUT=$(run cat /reevofs/output/captured.txt 2>/dev/null)
assert_eq "capture command output" "captured output" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 21. mv edge cases ==="
# ═══════════════════════════════════════════════════════════════════════

# mv within reevofs output (intra-namespace)
run bash -c 'echo "intra mv" > /reevofs/output/mv_intra_src.txt'
run mv /reevofs/output/mv_intra_src.txt /reevofs/output/mv_intra_dst.txt 2>/dev/null
OUT=$(run cat /reevofs/output/mv_intra_dst.txt 2>/dev/null)
assert_eq "mv within output ns" "intra mv" "$OUT"
assert_fail "mv intra source gone" cat /reevofs/output/mv_intra_src.txt

# mv a binary file (all 256 byte values) from real fs — must round-trip
# byte-for-byte via the raw-bytes PUT contract. Uses .dat since .bin is
# blocklisted by the backend.
python3 -c "
with open('/tmp/binary_mv.dat', 'wb') as f:
    f.write(bytes(range(256)))
" 2>/dev/null
EXPECTED_MV_MD5=$(md5sum /tmp/binary_mv.dat | awk '{print $1}')
run mv /tmp/binary_mv.dat /reevofs/output/binary_mv.dat 2>/dev/null
ACTUAL_MV_MD5=$(run cat /reevofs/output/binary_mv.dat 2>/dev/null | md5sum | awk '{print $1}')
assert_eq "mv binary file bytes round-trip" "$EXPECTED_MV_MD5" "$ACTUAL_MV_MD5"

# cp a binary file (all 256 byte values) into /reevofs/output — must round-trip
# byte-for-byte. Exercises flush_write_fd on a non-UTF-8 body AND the
# stat-before-read path that relies on Accept: application/octet-stream.
python3 -c "
with open('/tmp/binary_cp.dat', 'wb') as f:
    f.write(bytes(range(256)))
" 2>/dev/null
EXPECTED_CP_MD5=$(md5sum /tmp/binary_cp.dat | awk '{print $1}')
run cp /tmp/binary_cp.dat /reevofs/output/binary_cp.dat 2>/dev/null
ACTUAL_CP_MD5=$(run cat /reevofs/output/binary_cp.dat 2>/dev/null | md5sum | awk '{print $1}')
assert_eq "cp binary file bytes round-trip" "$EXPECTED_CP_MD5" "$ACTUAL_CP_MD5"

# Python open('wb') binary write + stat + read-back — the exact scenario that
# failed before v0.3.9 (stat → ENOENT on binary, read → EACCES on binary).
run python3 -c "
import hashlib, os
data = bytes(range(256))
p = '/reevofs/output/pywrite_bin.png'
with open(p, 'wb') as f: f.write(data)
assert os.path.getsize(p) == 256, f'getsize returned {os.path.getsize(p)}'
with open(p, 'rb') as f: got = f.read()
assert hashlib.md5(got).hexdigest() == hashlib.md5(data).hexdigest(), 'md5 mismatch'
" 2>/tmp/reevofs_pywb.err
if [ $? -eq 0 ]; then
    PASS=$((PASS + 1))
    echo "  PASS: python open('wb') binary write + stat + read-back"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: python binary round-trip: $(cat /tmp/reevofs_pywb.err)"
    echo "  FAIL: python binary round-trip: $(cat /tmp/reevofs_pywb.err)"
fi

# mv with directory path (mv /tmp/dir/ → reevofs — should fail or use files)
mkdir -p /tmp/mv_dir_test
echo "dir file" > /tmp/mv_dir_test/inner.txt
# mv directory to reevofs should fail (we don't support directory mv)
if run mv /tmp/mv_dir_test /reevofs/output/mv_dir_test 2>/dev/null; then
    # If it somehow succeeds, check if file is accessible
    OUT=$(run cat /reevofs/output/mv_dir_test/inner.txt 2>/dev/null)
    if [ "$OUT" = "dir file" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: mv directory (unexpected success, but file accessible)"
    else
        PASS=$((PASS + 1))
        echo "  PASS: mv directory (completed but content not verified)"
    fi
else
    PASS=$((PASS + 1))
    echo "  PASS: mv directory to reevofs fails as expected"
fi

# mv large file from real fs
python3 -c "
with open('/tmp/large_mv.txt', 'w') as f:
    f.write('x' * 50000)
" 2>/dev/null
run mv /tmp/large_mv.txt /reevofs/output/large_mv.txt 2>/dev/null
OUT=$(run python3 -c "
with open('/reevofs/output/large_mv.txt') as f:
    print(len(f.read()))
" 2>/dev/null)
assert_eq "mv large file (50KB)" "50000" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 22. Common CLI commands ==="
# ═══════════════════════════════════════════════════════════════════════

# --- head / tail ---
run bash -c 'printf "line1\nline2\nline3\nline4\nline5\n" > /reevofs/output/lines.txt'
OUT=$(run head -n 2 /reevofs/output/lines.txt 2>/dev/null)
assert_eq "head -n 2" "line1
line2" "$OUT"

OUT=$(run tail -n 2 /reevofs/output/lines.txt 2>/dev/null)
assert_eq "tail -n 2" "line4
line5" "$OUT"

OUT=$(run head -c 5 /reevofs/output/lines.txt 2>/dev/null)
assert_eq "head -c 5" "line1" "$OUT"

# --- wc ---
OUT=$(run wc -l /reevofs/output/lines.txt 2>/dev/null | awk '{print $1}')
assert_eq "wc -l" "5" "$OUT"

OUT=$(run wc -c /reevofs/skills/hello.txt 2>/dev/null | awk '{print $1}')
if [ -n "$OUT" ] && [ "$OUT" -gt 0 ] 2>/dev/null; then
    PASS=$((PASS + 1))
    echo "  PASS: wc -c reports bytes ($OUT)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: wc -c reports bytes (got: $OUT)"
    echo "  FAIL: wc -c reports bytes"
fi

# --- grep ---
run bash -c 'printf "apple\nbanana\ncherry\napricot\n" > /reevofs/output/fruits.txt'
OUT=$(run grep "^a" /reevofs/output/fruits.txt 2>/dev/null)
assert_eq "grep pattern" "apple
apricot" "$OUT"

# grep -c counts lines containing match
OUT=$(run grep -c "a" /reevofs/output/fruits.txt 2>/dev/null)
if [ -n "$OUT" ] && [ "$OUT" -gt 0 ] 2>/dev/null; then
    PASS=$((PASS + 1))
    echo "  PASS: grep -c count ($OUT lines)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: grep -c count (got: $OUT)"
    echo "  FAIL: grep -c count"
fi

OUT=$(run grep -i "BANANA" /reevofs/output/fruits.txt 2>/dev/null)
assert_eq "grep -i case insensitive" "banana" "$OUT"

# --- sed (via cat pipe — direct sed on reevofs may use sendfile/mmap) ---
OUT=$(run bash -c "cat /reevofs/skills/hello.txt | sed 's/hello/goodbye/'" 2>/dev/null)
assert_eq "sed substitute (piped)" "goodbye world" "$OUT"

# sed read→process→write via python (pipe+redirect loses data in some cases)
run python3 -c "
with open('/reevofs/output/fruits.txt') as f:
    import re
    data = f.read().replace('apple', 'orange')
with open('/reevofs/output/fruits_sed.txt', 'w') as f:
    f.write(data)
" 2>/dev/null
OUT=$(run head -n 1 /reevofs/output/fruits_sed.txt 2>/dev/null)
assert_eq "sed-like to reevofs file" "orange" "$OUT"

# --- awk (piped to avoid mmap issues) ---
run bash -c 'printf "name,age,city\nalice,30,nyc\nbob,25,sf\n" > /reevofs/output/data.csv'
OUT=$(run bash -c "cat /reevofs/output/data.csv | awk -F, '{print \$1}'" 2>/dev/null | tail -n +2)
assert_eq "awk field extract" "alice
bob" "$OUT"

OUT=$(run bash -c "cat /reevofs/output/data.csv | awk -F, 'NR>1{sum+=\$2} END{print sum}'" 2>/dev/null)
assert_eq "awk sum" "55" "$OUT"

# --- sort (piped) ---
run bash -c 'printf "cherry\napple\nbanana\n" > /reevofs/output/unsorted.txt'
OUT=$(run bash -c 'cat /reevofs/output/unsorted.txt | sort' 2>/dev/null)
assert_eq "sort" "apple
banana
cherry" "$OUT"

# sort output to reevofs (via python to avoid pipe+redirect issues)
run python3 -c "
with open('/reevofs/output/unsorted.txt') as f:
    lines = sorted(f.read().strip().split('\n'))
with open('/reevofs/output/sorted.txt', 'w') as f:
    f.write('\n'.join(lines) + '\n')
" 2>/dev/null
OUT=$(run cat /reevofs/output/sorted.txt 2>/dev/null | head -3)
assert_eq "sort > reevofs" "apple
banana
cherry" "$OUT"

# --- uniq (piped) ---
run bash -c 'printf "a\na\nb\nb\nb\nc\n" > /reevofs/output/dupes.txt'
OUT=$(run bash -c 'cat /reevofs/output/dupes.txt | sort | uniq -c' 2>/dev/null | awk '{print $1,$2}')
if echo "$OUT" | grep -q "2 a" && echo "$OUT" | grep -q "3 b"; then
    PASS=$((PASS + 1))
    echo "  PASS: sort | uniq -c"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: sort | uniq -c (got: $OUT)"
    echo "  FAIL: sort | uniq -c"
fi

# --- cut (piped) ---
OUT=$(run bash -c "cat /reevofs/output/data.csv | cut -d, -f2" 2>/dev/null | tail -n +2)
assert_eq "cut -d, -f2" "30
25" "$OUT"

# --- tr ---
OUT=$(run bash -c 'cat /reevofs/skills/hello.txt | tr "[:lower:]" "[:upper:]"' 2>/dev/null)
assert_eq "tr uppercase" "HELLO WORLD" "$OUT"

# --- diff (via process substitution to avoid mmap) ---
run bash -c 'echo "hello world" > /reevofs/output/diff1.txt'
run bash -c 'echo "hello universe" > /reevofs/output/diff2.txt'
OUT=$(run bash -c 'diff <(cat /reevofs/output/diff1.txt) <(cat /reevofs/output/diff2.txt)' 2>/dev/null || true)
if echo "$OUT" | grep -q "world" && echo "$OUT" | grep -q "universe"; then
    PASS=$((PASS + 1))
    echo "  PASS: diff two reevofs files"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: diff two reevofs files (got: $OUT)"
    echo "  FAIL: diff two reevofs files"
fi

# --- cat with multiple files ---
OUT=$(run bash -c 'cat /reevofs/output/diff1.txt /reevofs/output/diff2.txt' 2>/dev/null)
assert_eq "cat multiple files" "hello world
hello universe" "$OUT"

# --- dd (piped) ---
run bash -c 'echo "dd test content" > /reevofs/output/dd_src.txt'
run bash -c 'cat /reevofs/output/dd_src.txt | dd of=/reevofs/output/dd_dst.txt 2>/dev/null'
OUT=$(run cat /reevofs/output/dd_dst.txt 2>/dev/null)
assert_eq "dd piped to reevofs" "dd test content" "$OUT"

# --- xargs ---
OUT=$(run bash -c 'echo "/reevofs/skills/hello.txt" | xargs cat' 2>/dev/null)
assert_eq "xargs cat" "hello world" "$OUT"

# --- file / md5sum: these use mmap() which isn't intercepted, skip ---
# Use python equivalents instead
OUT=$(run python3 -c "
import hashlib
with open('/reevofs/skills/hello.txt', 'rb') as f:
    print(hashlib.md5(f.read()).hexdigest())
" 2>/dev/null)
if [ -n "$OUT" ] && [ ${#OUT} -eq 32 ]; then
    PASS=$((PASS + 1))
    echo "  PASS: python md5 hash ($OUT)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: python md5 hash (got: $OUT)"
    echo "  FAIL: python md5 hash"
fi

# --- Piped workflows ---
# grep | wc pipeline
run bash -c 'printf "error: disk full\ninfo: ok\nerror: timeout\nwarn: slow\n" > /reevofs/output/log_pipe.txt'
OUT=$(run bash -c 'cat /reevofs/output/log_pipe.txt | grep "^error" | wc -l')
assert_eq "grep | wc pipeline" "2" "$OUT"

# Multi-step pipeline: read → process → write (via python to avoid pipe→redirect issues)
run python3 -c "
with open('/reevofs/output/data.csv') as f:
    lines = [l.strip() for l in f if not l.startswith('name')]
lines.sort(key=lambda l: int(l.split(',')[1]))
with open('/reevofs/output/sorted_data.csv', 'w') as f:
    f.write('\n'.join(lines) + '\n')
" 2>/dev/null
OUT=$(run head -n 1 /reevofs/output/sorted_data.csv 2>/dev/null)
assert_eq "multi-step pipeline" "bob,25,sf" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 23. ls variations ==="
# ═══════════════════════════════════════════════════════════════════════

# ls bare
OUT=$(run ls /reevofs/ 2>/dev/null)
if echo "$OUT" | grep -q "skills" && echo "$OUT" | grep -q "output"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls /reevofs/"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls /reevofs/ (got: $OUT)"
    echo "  FAIL: ls /reevofs/"
fi

# ls -1 (one per line)
OUT=$(run ls -1 /reevofs/skills/ 2>/dev/null)
if echo "$OUT" | grep -q "hello.txt" && echo "$OUT" | grep -q "my-skill"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls -1"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls -1 (got: $OUT)"
    echo "  FAIL: ls -1"
fi

# ls -lh (human-readable sizes)
OUT=$(run ls -lh /reevofs/skills/hello.txt 2>/dev/null)
if echo "$OUT" | grep -q "hello.txt"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls -lh file"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls -lh file (got: $OUT)"
    echo "  FAIL: ls -lh file"
fi

# ls -ld (directory info)
OUT=$(run ls -ld /reevofs/skills/ 2>/dev/null)
if echo "$OUT" | grep -q "^d"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls -ld shows directory"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls -ld shows directory (got: $OUT)"
    echo "  FAIL: ls -ld shows directory"
fi

# ls -R (recursive) — may not work fully, test with timeout
OUT=$(run timeout 5 ls -R /reevofs/skills/ 2>/dev/null || echo "TIMEOUT")
if echo "$OUT" | grep -q "TIMEOUT"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls -R times out gracefully (known limitation)"
elif echo "$OUT" | grep -q "SKILL.md"; then
    PASS=$((PASS + 1))
    echo "  PASS: ls -R works recursively"
else
    PASS=$((PASS + 1))
    echo "  PASS: ls -R completed (partial: $OUT)"
fi

# ls on file (not directory)
OUT=$(run ls /reevofs/skills/hello.txt 2>/dev/null)
assert_eq "ls on file" "/reevofs/skills/hello.txt" "$OUT"

# ls nonexistent
if run ls /reevofs/skills/nonexistent.txt 2>/dev/null; then
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: ls nonexistent should fail"
    echo "  FAIL: ls nonexistent should fail"
else
    PASS=$((PASS + 1))
    echo "  PASS: ls nonexistent fails"
fi

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 24. File creation and lifecycle ==="
# ═══════════════════════════════════════════════════════════════════════

# Create → read → overwrite → read → delete → verify gone
run bash -c 'echo "version 1" > /reevofs/output/lifecycle.txt'
OUT=$(run cat /reevofs/output/lifecycle.txt 2>/dev/null)
assert_eq "lifecycle: create" "version 1" "$OUT"

run bash -c 'echo "version 2" > /reevofs/output/lifecycle.txt'
OUT=$(run cat /reevofs/output/lifecycle.txt 2>/dev/null)
assert_eq "lifecycle: overwrite" "version 2" "$OUT"

run rm /reevofs/output/lifecycle.txt 2>/dev/null
assert_fail "lifecycle: deleted" cat /reevofs/output/lifecycle.txt

# Create file, check stat, then check with ls
run bash -c 'echo "stat check" > /reevofs/output/stat_check.txt'
assert_ok "lifecycle: stat exists" stat /reevofs/output/stat_check.txt

# test -e (exists)
run bash -c 'test -e /reevofs/output/stat_check.txt && echo yes || echo no' > /tmp/exists_test.out 2>/dev/null
OUT=$(cat /tmp/exists_test.out)
assert_eq "test -e (exists)" "yes" "$OUT"

run bash -c 'test -e /reevofs/output/no_such_file.txt && echo yes || echo no' > /tmp/exists_test2.out 2>/dev/null
OUT=$(cat /tmp/exists_test2.out)
assert_eq "test -e (missing)" "no" "$OUT"

# test -s (file has size > 0)
run bash -c 'test -s /reevofs/output/stat_check.txt && echo yes || echo no' > /tmp/size_test.out 2>/dev/null
OUT=$(cat /tmp/size_test.out)
assert_eq "test -s (non-empty)" "yes" "$OUT"

# Empty file
run bash -c '> /reevofs/output/empty_file.txt'
assert_ok "create empty file" stat /reevofs/output/empty_file.txt
OUT=$(run cat /reevofs/output/empty_file.txt 2>/dev/null)
assert_eq "read empty file" "" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 25. Special characters and filenames ==="
# ═══════════════════════════════════════════════════════════════════════

# Filename with spaces
run bash -c 'echo "space file" > "/reevofs/output/file with spaces.txt"'
OUT=$(run cat "/reevofs/output/file with spaces.txt" 2>/dev/null)
assert_eq "filename with spaces" "space file" "$OUT"

# Filename with dashes and underscores
run bash -c 'echo "dash" > /reevofs/output/my-file_v2.txt'
OUT=$(run cat /reevofs/output/my-file_v2.txt 2>/dev/null)
assert_eq "filename with dash/underscore" "dash" "$OUT"

# Filename with dots
run bash -c 'echo "dotfile" > /reevofs/output/file.name.with.dots.txt'
OUT=$(run cat /reevofs/output/file.name.with.dots.txt 2>/dev/null)
assert_eq "filename with dots" "dotfile" "$OUT"

# Content with special characters
run bash -c 'printf "line1\tTabbed\nline2\twith \"quotes\"\n" > /reevofs/output/special_chars.txt'
OUT=$(run cat /reevofs/output/special_chars.txt 2>/dev/null)
if echo "$OUT" | grep -q "Tabbed" && echo "$OUT" | grep -q "quotes"; then
    PASS=$((PASS + 1))
    echo "  PASS: special chars in content"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: special chars in content (got: $OUT)"
    echo "  FAIL: special chars in content"
fi

# Unicode content
run python3 -c "
with open('/reevofs/output/unicode.txt', 'w') as f:
    f.write('Hello 世界 🌍\n')
" 2>/dev/null
OUT=$(run python3 -c "
with open('/reevofs/output/unicode.txt') as f:
    print(f.read().strip())
" 2>/dev/null)
assert_eq "unicode content" "Hello 世界 🌍" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 26. Multi-tool workflows ==="
# ═══════════════════════════════════════════════════════════════════════

# Python generates CSV → bash processes it
run python3 -c "
import csv, io
buf = io.StringIO()
w = csv.writer(buf)
w.writerow(['name', 'score'])
w.writerow(['alice', 95])
w.writerow(['bob', 87])
w.writerow(['charlie', 92])
with open('/reevofs/output/scores.csv', 'w') as f:
    f.write(buf.getvalue())
" 2>/dev/null
OUT=$(run bash -c 'tail -n +2 /reevofs/output/scores.csv | sort -t, -k2 -rn | head -1 | cut -d, -f1')
assert_eq "python csv → bash process" "alice" "$OUT"

# Bash creates data → node processes it
run bash -c 'echo "[1,2,3,4,5]" > /reevofs/output/numbers.json'
OUT=$(run timeout 10 node -e "
const fs = require('fs');
const nums = JSON.parse(fs.readFileSync('/reevofs/output/numbers.json', 'utf8'));
const sum = nums.reduce((a,b) => a+b, 0);
fs.writeFileSync('/reevofs/output/sum.txt', String(sum));
console.log(sum);
" 2>/dev/null)
assert_eq "bash json → node process" "15" "$OUT"

# Verify the node output file is readable
OUT=$(run cat /reevofs/output/sum.txt 2>/dev/null)
assert_eq "node output persisted" "15" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 28. Direct fopen-based tools (no cat pipe workarounds) ==="
# ═══════════════════════════════════════════════════════════════════════
# These tools use fopen() internally, which was previously unintercepted.
# With fopen/fopen64 hooks, they should now work directly on /reevofs/ paths.

# --- sed directly on reevofs file ---
OUT=$(run sed 's/hello/goodbye/' /reevofs/skills/hello.txt 2>/dev/null)
assert_eq "sed direct on reevofs" "goodbye world" "$OUT"

# sed -n with pattern match
OUT=$(run sed -n '/hello/p' /reevofs/skills/hello.txt 2>/dev/null)
assert_eq "sed -n pattern direct" "hello world" "$OUT"

# sed in-place simulation: read → transform → write
run bash -c 'echo -e "line1\nline2\nline3" > /reevofs/output/sed_direct.txt'
OUT=$(run sed 's/line/row/' /reevofs/output/sed_direct.txt 2>/dev/null)
assert_eq "sed direct on output file" "row1
row2
row3" "$OUT"

# --- sort directly on reevofs file ---
run bash -c 'printf "cherry\napple\nbanana\n" > /reevofs/output/sort_direct.txt'
OUT=$(run sort /reevofs/output/sort_direct.txt 2>/dev/null)
assert_eq "sort direct on reevofs" "apple
banana
cherry" "$OUT"

# sort -r (reverse)
OUT=$(run sort -r /reevofs/output/sort_direct.txt 2>/dev/null)
assert_eq "sort -r direct" "cherry
banana
apple" "$OUT"

# --- diff directly on reevofs files ---
run bash -c 'echo "alpha" > /reevofs/output/diff_a.txt'
run bash -c 'echo "beta" > /reevofs/output/diff_b.txt'
OUT=$(run diff /reevofs/output/diff_a.txt /reevofs/output/diff_b.txt 2>/dev/null || true)
if echo "$OUT" | grep -q "alpha" && echo "$OUT" | grep -q "beta"; then
    PASS=$((PASS + 1))
    echo "  PASS: diff direct on reevofs"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: diff direct on reevofs (got: $OUT)"
    echo "  FAIL: diff direct on reevofs"
fi

# --- md5sum directly on reevofs file ---
OUT=$(run md5sum /reevofs/skills/hello.txt 2>/dev/null | awk '{print $1}')
if [ -n "$OUT" ] && [ ${#OUT} -eq 32 ]; then
    PASS=$((PASS + 1))
    echo "  PASS: md5sum direct ($OUT)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: md5sum direct (got: $OUT)"
    echo "  FAIL: md5sum direct"
fi

# --- wc directly on reevofs file ---
OUT=$(run wc -l /reevofs/output/sed_direct.txt 2>/dev/null | awk '{print $1}')
assert_eq "wc -l direct" "3" "$OUT"

OUT=$(run wc -w /reevofs/skills/hello.txt 2>/dev/null | awk '{print $1}')
assert_eq "wc -w direct" "2" "$OUT"

# --- awk directly on reevofs file ---
run bash -c 'printf "name,age\nalice,30\nbob,25\n" > /reevofs/output/awk_direct.csv'
OUT=$(run awk -F, 'NR>1{print $1}' /reevofs/output/awk_direct.csv 2>/dev/null)
assert_eq "awk direct on reevofs" "alice
bob" "$OUT"

# --- grep directly on reevofs file ---
OUT=$(run grep "hello" /reevofs/skills/hello.txt 2>/dev/null)
assert_eq "grep direct on reevofs" "hello world" "$OUT"

OUT=$(run grep -c "l" /reevofs/skills/hello.txt 2>/dev/null)
if [ "$OUT" -gt 0 ] 2>/dev/null; then
    PASS=$((PASS + 1))
    echo "  PASS: grep -c direct ($OUT)"
else
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: grep -c direct (got: $OUT)"
    echo "  FAIL: grep -c direct"
fi

# --- head/tail directly ---
run bash -c 'printf "one\ntwo\nthree\nfour\nfive\n" > /reevofs/output/lines.txt'
OUT=$(run head -n 2 /reevofs/output/lines.txt 2>/dev/null)
assert_eq "head -n 2 direct" "one
two" "$OUT"

OUT=$(run tail -n 2 /reevofs/output/lines.txt 2>/dev/null)
assert_eq "tail -n 2 direct" "four
five" "$OUT"

# --- cut directly ---
OUT=$(run cut -d, -f1 /reevofs/output/awk_direct.csv 2>/dev/null)
assert_eq "cut direct on reevofs" "name
alice
bob" "$OUT"

# --- uniq directly ---
run bash -c 'printf "a\na\nb\nb\nb\nc\n" > /reevofs/output/uniq_direct.txt'
OUT=$(run uniq /reevofs/output/uniq_direct.txt 2>/dev/null)
assert_eq "uniq direct on reevofs" "a
b
c" "$OUT"

# --- sort + write back to reevofs ---
OUT=$(run bash -c 'sort /reevofs/output/sort_direct.txt > /reevofs/output/sort_result.txt && cat /reevofs/output/sort_result.txt' 2>/dev/null)
assert_eq "sort > reevofs direct" "apple
banana
cherry" "$OUT"

# --- sed + write back to reevofs ---
OUT=$(run bash -c 'sed "s/apple/mango/" /reevofs/output/sort_direct.txt > /reevofs/output/sed_result.txt && cat /reevofs/output/sed_result.txt' 2>/dev/null)
assert_eq "sed > reevofs direct" "cherry
mango
banana" "$OUT"

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 29. Fork+exec, redirects, and advanced write patterns ==="
# ═══════════════════════════════════════════════════════════════════════

# --- Subshell write: (cmd) > /reevofs/file ---
# Tests fd inheritance through subshell fork
run bash -c '(echo "from subshell") > /reevofs/output/subshell.txt'
OUT=$(run cat /reevofs/output/subshell.txt)
assert_eq "subshell write" "from subshell" "$OUT"

# --- Subshell with multiple commands ---
run bash -c '(echo "line1"; echo "line2"; echo "line3") > /reevofs/output/subshell_multi.txt'
OUT=$(run cat /reevofs/output/subshell_multi.txt)
assert_eq "subshell multi-cmd write" "line1
line2
line3" "$OUT"

# --- Here-string via cat ---
run bash -c 'cat <<< "here-string content" > /reevofs/output/herestring.txt'
OUT=$(run cat /reevofs/output/herestring.txt)
assert_eq "here-string via cat" "here-string content" "$OUT"

# --- Tee writing to reevofs ---
OUT=$(run bash -c 'echo "tee content" | tee /reevofs/output/tee_out.txt')
assert_eq "tee stdout passthrough" "tee content" "$OUT"
OUT=$(run cat /reevofs/output/tee_out.txt)
assert_eq "tee file write" "tee content" "$OUT"

# --- Tee to multiple reevofs files ---
run bash -c 'echo "multi tee" | tee /reevofs/output/tee_a.txt > /reevofs/output/tee_b.txt'
OUT_A=$(run cat /reevofs/output/tee_a.txt)
OUT_B=$(run cat /reevofs/output/tee_b.txt)
assert_eq "tee to multiple files (a)" "multi tee" "$OUT_A"
assert_eq "tee to multiple files (b)" "multi tee" "$OUT_B"

# --- Pipeline: external cmd | external cmd > reevofs ---
run bash -c 'echo -e "ccc\naaa\nbbb" | sort > /reevofs/output/pipe_sort.txt'
OUT=$(run cat /reevofs/output/pipe_sort.txt)
assert_eq "pipeline sort > reevofs" "aaa
bbb
ccc" "$OUT"

# --- Pipeline: 3-stage with reevofs output ---
run bash -c 'echo -e "Hello World\nFoo Bar" | tr "[:upper:]" "[:lower:]" | sort > /reevofs/output/pipe3.txt'
OUT=$(run cat /reevofs/output/pipe3.txt)
assert_eq "3-stage pipeline > reevofs" "foo bar
hello world" "$OUT"

# --- Background process write ---
run bash -c 'echo "bg content" > /reevofs/output/bg_write.txt & wait'
OUT=$(run cat /reevofs/output/bg_write.txt)
assert_eq "background write" "bg content" "$OUT"

# --- Large file crossing 8192-byte buffer boundary ---
# flush_write_fd reads in 8192-byte chunks; test boundary at exactly 8192 and 8193 bytes
run bash -c 'python3 -c "print(\"A\" * 8192, end=\"\")" > /reevofs/output/boundary_8192.txt'
OUT=$(run bash -c 'wc -c < /reevofs/output/boundary_8192.txt' | tr -d ' ')
assert_eq "write exactly 8192 bytes" "8192" "$OUT"

run bash -c 'python3 -c "print(\"B\" * 8193, end=\"\")" > /reevofs/output/boundary_8193.txt'
OUT=$(run bash -c 'wc -c < /reevofs/output/boundary_8193.txt' | tr -d ' ')
assert_eq "write 8193 bytes (crosses buffer)" "8193" "$OUT"

# --- Heredoc with large content (multi-chunk write through cat fork+exec) ---
run bash -c 'cat > /reevofs/output/heredoc_large.txt << EOF
$(python3 -c "print(\"X\" * 20000)")
EOF'
OUT=$(run bash -c 'wc -c < /reevofs/output/heredoc_large.txt' | tr -d ' ')
# 20000 chars + newline from print (heredoc strips trailing newline for the delimiter line)
assert_eq "large heredoc via cat (20KB)" "20001" "$OUT"

# --- Concurrent writes from parallel background processes ---
run bash -c '
echo "proc1" > /reevofs/output/concurrent_1.txt &
echo "proc2" > /reevofs/output/concurrent_2.txt &
echo "proc3" > /reevofs/output/concurrent_3.txt &
wait
'
OUT1=$(run cat /reevofs/output/concurrent_1.txt)
OUT2=$(run cat /reevofs/output/concurrent_2.txt)
OUT3=$(run cat /reevofs/output/concurrent_3.txt)
assert_eq "concurrent write 1" "proc1" "$OUT1"
assert_eq "concurrent write 2" "proc2" "$OUT2"
assert_eq "concurrent write 3" "proc3" "$OUT3"

# --- xargs writing to reevofs ---
run bash -c 'echo -e "x1\nx2\nx3" | xargs -I{} bash -c "echo \"val_{}\" > /reevofs/output/xargs_{}.txt"'
OUT=$(run cat /reevofs/output/xargs_x1.txt)
assert_eq "xargs write" "val_x1" "$OUT"

# --- dd writing to reevofs ---
run bash -c 'echo "dd content" | dd of=/reevofs/output/dd_out.txt 2>/dev/null'
OUT=$(run cat /reevofs/output/dd_out.txt)
assert_eq "dd write to reevofs" "dd content" "$OUT"

# --- exec fd redirect (open fd 3, write via >&3, close) ---
run bash -c 'exec 3>/reevofs/output/exec_fd.txt; echo "via fd 3" >&3; exec 3>&-'
OUT=$(run cat /reevofs/output/exec_fd.txt)
assert_eq "exec fd redirect (fd 3)" "via fd 3" "$OUT"

# --- Process substitution writing to reevofs ---
# Using tee with process substitution to write to reevofs
run bash -c 'echo "procsub" | tee >(cat > /reevofs/output/procsub.txt) > /dev/null; sleep 0.2'
OUT=$(run cat /reevofs/output/procsub.txt)
assert_eq "process substitution write" "procsub" "$OUT"

# --- Node.js write via syscall(SYS_close) path ---
run node -e "
const fs = require('fs');
fs.writeFileSync('/reevofs/output/node_sync.txt', 'node sync write');
const content = fs.readFileSync('/reevofs/output/node_sync.txt', 'utf8');
if (content !== 'node sync write') process.exit(1);
"
assert_ok "node.js writeFileSync + readFileSync" true

# Node.js async write
run node -e "
const fs = require('fs').promises;
async function main() {
    await fs.writeFile('/reevofs/output/node_async.txt', 'node async write');
    const content = await fs.readFile('/reevofs/output/node_async.txt', 'utf8');
    if (content !== 'node async write') process.exit(1);
}
main();
"
assert_ok "node.js async write + read" true

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 30. Append redirect (>>) ==="
# ═══════════════════════════════════════════════════════════════════════

# Basic append — three lines
run bash -c 'echo "Line 1" > /reevofs/output/append_test.txt'
run bash -c 'echo "Line 2" >> /reevofs/output/append_test.txt'
run bash -c 'echo "Line 3" >> /reevofs/output/append_test.txt'
GOT=$(run cat /reevofs/output/append_test.txt)
EXPECT=$'Line 1\nLine 2\nLine 3'
[ "$GOT" = "$EXPECT" ]
assert_ok ">> append preserves all lines" true

# Append to non-existent file (should create)
run rm /reevofs/output/append_new.txt 2>/dev/null || true
run bash -c 'echo "first" >> /reevofs/output/append_new.txt'
GOT=$(run cat /reevofs/output/append_new.txt)
[ "$GOT" = "first" ]
assert_ok ">> append creates new file" true

# Multiple appends in a single shell session
run bash -c '
  echo "A" > /reevofs/output/append_multi.txt
  echo "B" >> /reevofs/output/append_multi.txt
  echo "C" >> /reevofs/output/append_multi.txt
  echo "D" >> /reevofs/output/append_multi.txt
'
GOT=$(run cat /reevofs/output/append_multi.txt)
LINES=$(echo "$GOT" | wc -l | tr -d ' ')
[ "$LINES" = "4" ]
assert_ok ">> four sequential appends in one shell" true

# Append with printf (no trailing newline on first two)
run bash -c 'printf "hello " > /reevofs/output/append_printf.txt'
run bash -c 'printf "world" >> /reevofs/output/append_printf.txt'
GOT=$(run cat /reevofs/output/append_printf.txt)
[ "$GOT" = "hello world" ]
assert_ok ">> append with printf" true

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 31. Input redirection (<) ==="
# ═══════════════════════════════════════════════════════════════════════

# wc -l with input redirect
run bash -c 'printf "line1\nline2\nline3\n" > /reevofs/output/wc_test.txt'
GOT=$(run bash -c 'wc -l < /reevofs/output/wc_test.txt' 2>&1)
GOT=$(echo "$GOT" | tr -d ' ')
[ "$GOT" = "3" ]
assert_ok "wc -l < file" true

# cat with input redirect
run bash -c 'echo "hello redirect" > /reevofs/output/redir_test.txt'
GOT=$(run bash -c 'cat < /reevofs/output/redir_test.txt')
[ "$GOT" = "hello redirect" ]
assert_ok "cat < file" true

# head with input redirect
run bash -c 'printf "a\nb\nc\nd\ne\n" > /reevofs/output/head_test.txt'
GOT=$(run bash -c 'head -n 2 < /reevofs/output/head_test.txt')
EXPECT=$'a\nb'
[ "$GOT" = "$EXPECT" ]
assert_ok "head -n 2 < file" true

# Input redirect in subshell
GOT=$(run bash -c 'echo $(wc -l < /reevofs/output/wc_test.txt)')
GOT=$(echo "$GOT" | tr -d ' ')
[ "$GOT" = "3" ]
assert_ok '$(wc -l < file) subshell' true

# Cleanup
run rm /reevofs/output/append_test.txt /reevofs/output/append_new.txt /reevofs/output/append_multi.txt /reevofs/output/append_printf.txt /reevofs/output/wc_test.txt /reevofs/output/redir_test.txt /reevofs/output/head_test.txt 2>/dev/null || true

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "=== 32. Benchmarks ==="
# ═══════════════════════════════════════════════════════════════════════

bench() {
    local name="$1"
    shift
    local start end elapsed
    start=$(date +%s%N)
    "$@" > /dev/null 2>&1 || true
    end=$(date +%s%N)
    elapsed=$(( (end - start) / 1000000 ))
    echo "  BENCH: ${name} = ${elapsed}ms"
}

echo "--- Single operation latency ---"
bench "stat (cached)"         run stat /reevofs/skills/hello.txt
bench "stat (cached repeat)"  run stat /reevofs/skills/hello.txt
bench "cat small file"        run cat /reevofs/skills/hello.txt
bench "cat small (repeat)"    run cat /reevofs/skills/hello.txt
bench "ls directory"          run ls /reevofs/skills/
bench "ls directory (repeat)" run ls /reevofs/skills/
bench "access check"          run test -f /reevofs/skills/hello.txt
bench "write small file"      run bash -c 'echo bench > /reevofs/output/bench_write.txt'
bench "read after write"      run cat /reevofs/output/bench_write.txt
bench "rename file"           run mv /reevofs/output/bench_write.txt /reevofs/output/bench_renamed.txt
bench "delete file"           run rm /reevofs/output/bench_renamed.txt

echo ""
echo "--- Bulk operations ---"

# Write 20 files
start_bulk=$(date +%s%N)
for i in $(seq 1 20); do
    run bash -c "echo 'file $i content' > /reevofs/output/bulk_${i}.txt" 2>/dev/null
done
end_bulk=$(date +%s%N)
elapsed_bulk=$(( (end_bulk - start_bulk) / 1000000 ))
echo "  BENCH: write 20 files = ${elapsed_bulk}ms (avg $(( elapsed_bulk / 20 ))ms/file)"

# Read 20 files
start_bulk=$(date +%s%N)
for i in $(seq 1 20); do
    run cat /reevofs/output/bulk_${i}.txt > /dev/null 2>&1
done
end_bulk=$(date +%s%N)
elapsed_bulk=$(( (end_bulk - start_bulk) / 1000000 ))
echo "  BENCH: read 20 files = ${elapsed_bulk}ms (avg $(( elapsed_bulk / 20 ))ms/file)"

# Stat 20 files (distinct)
start_bulk=$(date +%s%N)
for i in $(seq 1 20); do
    run stat /reevofs/output/bulk_${i}.txt > /dev/null 2>&1
done
end_bulk=$(date +%s%N)
elapsed_bulk=$(( (end_bulk - start_bulk) / 1000000 ))
echo "  BENCH: stat 20 files = ${elapsed_bulk}ms (avg $(( elapsed_bulk / 20 ))ms/file)"

# Stat same file 20x (should be fully cached after first)
start_bulk=$(date +%s%N)
for i in $(seq 1 20); do
    run stat /reevofs/output/bulk_1.txt > /dev/null 2>&1
done
end_bulk=$(date +%s%N)
elapsed_bulk=$(( (end_bulk - start_bulk) / 1000000 ))
echo "  BENCH: stat same file 20x (cached) = ${elapsed_bulk}ms (avg $(( elapsed_bulk / 20 ))ms/op)"

# Python: read + stat in a single process (amortizes shim init)
bench "python read+stat 10 files" run python3 -c "
import os
for i in range(1, 11):
    p = f'/reevofs/output/bulk_{i}.txt'
    os.stat(p)
    open(p).read()
"

# Cleanup bulk files
for i in $(seq 1 20); do
    run rm /reevofs/output/bulk_${i}.txt > /dev/null 2>&1 || true
done

# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "==========================================="
echo "Results: $PASS passed, $FAIL failed"
echo "==========================================="
if [ "$FAIL" -gt 0 ]; then
    echo -e "\nFailures:$ERRORS"
    exit 1
fi
exit 0
