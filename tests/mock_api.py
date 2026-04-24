#!/usr/bin/env python3
"""
Mock Reevo API server for integration testing.

Mirrors the real salestech-be contract documented in
salestech_be/web/api/fs/views.py:
  GET    /api/v2/fs/{ns}/{scope}/{path}   → read file (content negotiation
                                            via Accept header)
  PUT    /api/v2/fs/{ns}/{scope}/{path}   → write raw bytes
  DELETE /api/v2/fs/{ns}/{scope}/{path}   → delete file
  POST   /api/v2/fs/{ns}/{scope}/_list    → list directory

Content negotiation (read path): if the client sends
`Accept: application/octet-stream` (exact match, no wildcards), the body is
returned as raw bytes. Otherwise the legacy JSON `{path, content}` envelope
is returned — which requires the content to be valid UTF-8. Non-UTF-8 content
returns 415 with a message telling the caller to retry with
`Accept: application/octet-stream`.

Blocked extensions (real backend's blocklist — .bin is included): any path
ending in these returns 415 on both read and write regardless of content
or Accept header.
"""

import json
import sys
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import unquote

# Matches salestech_be/web/api/fs/mime.py::_BLOCKED_EXTENSIONS.
BLOCKED_EXT = (".exe", ".sh", ".bat", ".bin", ".dll", ".so", ".dylib")

# In-memory filesystem: { "skills/overlay/my-skill/SKILL.md": "content", ... }
FS = {}

# Pre-seed some test data
SEED = {
    "skills/overlay/my-skill/SKILL.md": "# My Skill\nThis is a test skill.",
    "skills/overlay/my-skill/config.json": '{"name": "my-skill", "version": "1.0"}',
    "skills/overlay/another-skill/README.md": "# Another Skill",
    "skills/overlay/hello.txt": "hello world",
    # Raw-bytes test: 4 non-UTF-8 bytes must round-trip exactly. Uses .dat
    # because .bin is on the blocklist (matches real backend).
    "skills/overlay/binary.dat": bytes([0xff, 0xfe, 0xfd, 0xfc]),
    # NOTE: output namespace is intentionally NOT pre-seeded.
    # The real API returns 404 for empty namespaces, and our shim must
    # handle this by treating configured namespace roots as always-existing
    # directories. Pre-seeding output data here would mask this bug.
}


def _wants_octet_stream(accept: str | None) -> bool:
    """Match salestech_be/web/api/fs/views.py::_wants_octet_stream.

    Only an explicit `application/octet-stream` in the Accept header opts in
    to raw bytes. Missing header, `*/*`, `application/json`, and any other
    value fall back to the legacy JSON envelope.
    """
    if not accept:
        return False
    for part in accept.split(","):
        media = part.split(";", maxsplit=1)[0].strip().lower()
        if media == "application/octet-stream":
            return True
    return False


def parse_fs_path(path: str):
    """Parse /api/v2/fs/{ns}/{scope}/{rest} → (ns, scope, rest)"""
    path = unquote(path)
    prefix = "/api/v2/fs/"
    if not path.startswith(prefix):
        return None, None, None
    rest = path[len(prefix):]
    parts = rest.split("/", 2)
    if len(parts) < 2:
        return None, None, None
    ns = parts[0]
    scope = parts[1]
    file_path = "/" + parts[2] if len(parts) > 2 else "/"
    return ns, scope, file_path


def fs_key(ns, scope, path):
    """Build the in-memory key: ns/scope/path (no leading slash)"""
    clean = path.strip("/")
    if clean:
        return f"{ns}/{scope}/{clean}"
    return f"{ns}/{scope}"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        # Log to stderr for debugging
        sys.stderr.write(f"[mock-api] {format % args}\n")
        sys.stderr.flush()

    def _json_response(self, code, data):
        body = json.dumps(data).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _problem_415(self, detail: str):
        body = json.dumps({
            "type": "about:blank",
            "title": "Unsupported Media Type",
            "status": 415,
            "detail": detail,
        }).encode()
        self.send_response(415)
        self.send_header("Content-Type", "application/problem+json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        ns, scope, path = parse_fs_path(self.path)
        if ns is None:
            self._json_response(400, {"error": "bad path"})
            return

        if path.endswith(BLOCKED_EXT):
            self._problem_415(
                f"File extension '{path.rsplit('.', 1)[-1]}'"
                f" is not served via this endpoint"
            )
            return

        key = fs_key(ns, scope, path)
        if key not in FS:
            self._json_response(404, {"error": "not found"})
            return

        body = FS[key]
        raw_bytes = body.encode("utf-8") if isinstance(body, str) else body
        wants_bytes = _wants_octet_stream(self.headers.get("Accept"))

        if wants_bytes:
            # Raw-bytes mode — exactly what the shim should be asking for.
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(raw_bytes)))
            self.end_headers()
            self.wfile.write(raw_bytes)
            return

        # Legacy JSON envelope. Requires UTF-8 content; fails 415 otherwise.
        try:
            text = raw_bytes.decode("utf-8")
        except UnicodeDecodeError:
            self._problem_415(
                f"File at {path!r} is not valid UTF-8 and cannot be returned"
                f" as JSON; retry with Accept: application/octet-stream to"
                f" receive raw bytes"
            )
            return

        resp = json.dumps({"path": path, "content": text}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def do_PUT(self):
        ns, scope, path = parse_fs_path(self.path)
        if ns is None:
            self._json_response(400, {"error": "bad path"})
            return

        if path.endswith(BLOCKED_EXT):
            self._problem_415(
                f"File extension '{path.rsplit('.', 1)[-1]}'"
                f" is not served via this endpoint"
            )
            return

        length = int(self.headers.get("Content-Length", 0))
        # New contract: raw bytes in the PUT body (Content-Type:
        # application/octet-stream). Storing bytes preserves binary files.
        content = self.rfile.read(length) if length else b""

        # Reject writes to scopes starting with "reject-" to simulate backend errors.
        if scope.startswith("reject-"):
            self._json_response(400, {"error": "invalid scope"})
            return

        key = fs_key(ns, scope, path)
        FS[key] = content
        self._json_response(200, {"success": True, "path": path})

    def do_DELETE(self):
        ns, scope, path = parse_fs_path(self.path)
        if ns is None:
            self._json_response(400, {"error": "bad path"})
            return

        key = fs_key(ns, scope, path)
        if key in FS:
            del FS[key]
            self._json_response(200, {"success": True, "path": path})
        else:
            self._json_response(404, {"error": "not found"})

    def do_POST(self):
        """Handle _list endpoint: POST /api/v2/fs/{ns}/{scope}/_list"""
        ns, scope, fpath = parse_fs_path(self.path)
        if ns is None:
            self._json_response(400, {"error": "bad path"})
            return

        # Check if this is a _list request
        if not self.path.rstrip("/").endswith("/_list"):
            self._json_response(400, {"error": "unknown POST endpoint"})
            return

        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length)) if length else {}
        list_path = body.get("path", "/").strip("/")

        # Find entries under ns/scope/list_path/
        prefix = f"{ns}/{scope}"
        if list_path:
            prefix = f"{prefix}/{list_path}"

        entries = {}
        for key in FS:
            if not key.startswith(prefix + "/") and key != prefix:
                continue
            rest = key[len(prefix):].strip("/")
            if not rest:
                continue
            # First component is the immediate child
            child = rest.split("/")[0]
            is_dir = "/" in rest
            # If we've seen this child as a dir, keep it as dir
            if child in entries:
                entries[child] = entries[child] or is_dir
            else:
                entries[child] = is_dir

        # NOTE: The real API returns 200 with empty entries for non-existent
        # paths (not 404). We match that behavior here so our tests catch bugs
        # where the shim incorrectly treats empty list_dir results as valid
        # directories (e.g. the cp/mv nested path bug with O_DIRECTORY probe).

        result = [{"name": name, "is_directory": is_dir} for name, is_dir in sorted(entries.items())]
        self._json_response(200, {"path": f"/{list_path}" if list_path else "/", "entries": result})


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9876
    FS.update(SEED)
    server = HTTPServer(("127.0.0.1", port), Handler)
    print(f"Mock API server running on http://127.0.0.1:{port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
