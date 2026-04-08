#!/usr/bin/env python3
"""
Mock Reevo API server for integration testing.
In-memory filesystem that implements:
  GET    /api/v2/fs/{ns}/{scope}/{path}   → read file
  PUT    /api/v2/fs/{ns}/{scope}/{path}   → write file
  DELETE /api/v2/fs/{ns}/{scope}/{path}   → delete file
  POST   /api/v2/fs/{ns}/{scope}/_list    → list directory
"""

import json
import sys
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import unquote

# In-memory filesystem: { "skills/overlay/my-skill/SKILL.md": "content", ... }
FS = {}

# Pre-seed some test data
SEED = {
    "skills/overlay/my-skill/SKILL.md": "# My Skill\nThis is a test skill.",
    "skills/overlay/my-skill/config.json": '{"name": "my-skill", "version": "1.0"}',
    "skills/overlay/another-skill/README.md": "# Another Skill",
    "skills/overlay/hello.txt": "hello world",
    "output/test-chat-id/existing.txt": "existing output content",
}


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

    def do_GET(self):
        ns, scope, path = parse_fs_path(self.path)
        if ns is None:
            self._json_response(400, {"error": "bad path"})
            return

        key = fs_key(ns, scope, path)
        if key in FS:
            self._json_response(200, {"path": path, "content": FS[key]})
        else:
            self._json_response(404, {"error": "not found"})

    def do_PUT(self):
        ns, scope, path = parse_fs_path(self.path)
        if ns is None:
            self._json_response(400, {"error": "bad path"})
            return

        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length)) if length else {}
        content = body.get("content", "")

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

        if not entries and list_path:
            # Non-root directory with no children doesn't exist.
            self._json_response(404, {"error": "not found"})
            return

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
