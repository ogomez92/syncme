"""Test agent for cross-machine runs of tests/e2e.py.

Run on the second machine:
    python3 tests/remote_agent.py <path-to-syncme-binary> <token>

It starts SyncMe (headless, port 47474, test data folder) and serves a small
control API on port 48600 so the test runner on the other machine can edit
files and call SyncMe's local API. Every request needs the token. All file
operations are confined to a fresh temp folder.
"""
import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

BIN = Path(sys.argv[1]).resolve()
TOKEN = sys.argv[2]
BASE = Path(tempfile.mkdtemp(prefix="syncme-remote-")).resolve()
SYNC_PORT = 47474
proc = None


def start():
    global proc
    if proc and proc.poll() is None:
        return
    log = open(BASE / "syncme.out", "ab")
    env = dict(os.environ, SYNCME_MUTE="1")
    proc = subprocess.Popen([str(BIN), "--headless", "--data-dir", str(BASE / "data"), "--port", str(SYNC_PORT), "--name", "Beta"],
                            stdout=log, stderr=log, env=env)


def stop():
    global proc
    if proc:
        proc.kill()
        proc.wait()
        proc = None


def inside(rel):
    p = (BASE / rel).resolve()
    if p != BASE and BASE not in p.parents:
        raise ValueError("path outside test folder")
    return p


def tree(root: Path):
    out = {}
    if not root.exists():
        return out
    for p in root.rglob("*"):
        rel = p.relative_to(root).as_posix()
        if rel.split("/")[0] == ".syncme":
            continue
        if p.is_file():
            out[rel] = hashlib.sha256(p.read_bytes()).hexdigest()
        elif p.is_dir():
            out[rel + "/"] = None
    return out


def api(path, body):
    data = None if body is None else json.dumps(body).encode()
    r = urllib.request.Request(f"http://127.0.0.1:{SYNC_PORT}{path}", data=data,
                               headers={"Content-Type": "application/json"} if body is not None else {})
    try:
        with urllib.request.urlopen(r, timeout=30) as resp:
            return 200, json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, {"error": e.read().decode()}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_POST(self):
        if self.headers.get("X-Token") != TOKEN:
            return self.reply(403, {"error": "bad token"})
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        try:
            op = body.get("op")
            if op == "info":
                return self.reply(200, {"base": str(BASE), "sep": os.sep, "os": sys.platform})
            if op == "api":
                code, res = api(body["path"], body.get("body"))
                return self.reply(code, res)
            if op == "start":
                start()
                return self.reply(200, {})
            if op == "stop":
                stop()
                return self.reply(200, {})
            if op == "tree":
                return self.reply(200, tree(inside(body["path"])))
            if op == "exists":
                return self.reply(200, {"exists": inside(body["path"]).exists()})
            if op == "read":
                return self.reply(200, {"b64": base64.b64encode(inside(body["path"]).read_bytes()).decode()})
            if op == "write":
                p = inside(body["path"])
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_bytes(base64.b64decode(body["b64"]))
                return self.reply(200, {})
            if op == "random":
                p = inside(body["path"])
                p.parent.mkdir(parents=True, exist_ok=True)
                data = os.urandom(body["size"])
                p.write_bytes(data)
                return self.reply(200, {"sha256": hashlib.sha256(data).hexdigest()})
            if op == "mkdir":
                inside(body["path"]).mkdir(parents=True, exist_ok=True)
                return self.reply(200, {})
            if op == "delete":
                inside(body["path"]).unlink()
                return self.reply(200, {})
            if op == "rmtree":
                shutil.rmtree(inside(body["path"]))
                return self.reply(200, {})
            if op == "rename":
                inside(body["path"]).rename(inside(body["to"]))
                return self.reply(200, {})
            if op == "glob":
                return self.reply(200, [p.relative_to(BASE).as_posix() for p in inside(body["path"]).rglob(body["pattern"])])
            return self.reply(400, {"error": f"unknown op {op}"})
        except Exception as e:  # noqa
            return self.reply(500, {"error": f"{type(e).__name__}: {e}"})


if __name__ == "__main__":
    start()
    print(f"SyncMe test agent: base {BASE}, control port 48600, SyncMe port {SYNC_PORT}", flush=True)
    try:
        ThreadingHTTPServer(("0.0.0.0", 48600), H).serve_forever()
    finally:
        stop()
