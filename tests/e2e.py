"""End-to-end sync tests: runs real SyncMe instances against real folders.

Same machine:   python tests/e2e.py [path-to-syncme-binary]
Two machines:   on the other machine run
                    python3 tests/remote_agent.py <syncme-binary> <token>
                then here
                    python tests/e2e.py <syncme-binary> --remote <ip> <token>

Every scenario checks that files only disappear when a device explicitly
deleted them, and that everything else ends up on every copy.
"""
import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

args = sys.argv[1:]
REMOTE = None
if "--remote" in args:
    i = args.index("--remote")
    REMOTE = (args[i + 1], args[i + 2])
    del args[i:i + 3]
BIN = Path(args[0] if args else "target/debug/syncme.exe").resolve()
ROOT = Path(tempfile.mkdtemp(prefix="syncme-e2e-")).resolve()
TIMEOUT = 60 if REMOTE else 40
results = []


def http_json(url, body=None, headers=None, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    h = dict(headers or {})
    if body is not None:
        h["Content-Type"] = "application/json"
    r = urllib.request.Request(url, data=data, headers=h)
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            return json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{url}: {e.code} {e.read().decode()}")


def sha(b):
    return hashlib.sha256(b).hexdigest()


class LocalNode:
    """A SyncMe instance on this machine; files live under ROOT/<name>."""

    def __init__(self, name, port):
        self.name, self.port = name, port
        self.base = ROOT / name.lower()
        self.proc = None
        self.id = None

    def start(self):
        env = dict(os.environ, SYNCME_MUTE="1")
        log = open(ROOT / f"{self.name}.out", "ab")
        self.proc = subprocess.Popen(
            [str(BIN), "--headless", "--data-dir", str(ROOT / f"data-{self.name}"), "--port", str(self.port), "--name", self.name],
            stdout=log, stderr=log, env=env)
        wait(lambda: self.try_state() is not None, f"{self.name} starts")
        self.id = self.state()["me"]["id"]

    def stop(self):
        if self.proc:
            self.proc.kill()
            self.proc.wait()
            self.proc = None

    def req(self, path, body=None):
        return http_json(f"http://127.0.0.1:{self.port}{path}", body)

    def abs(self, rel):
        return str(self.base / rel)

    def p(self, rel):
        return self.base / rel

    def write(self, rel, content):
        p = self.p(rel)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(content if isinstance(content, bytes) else content.encode())

    def random(self, rel, size):
        data = os.urandom(size)
        self.write(rel, data)
        return sha(data)

    def read(self, rel):
        return self.p(rel).read_bytes()

    def exists(self, rel):
        return self.p(rel).exists()

    def mkdir(self, rel):
        self.p(rel).mkdir(parents=True, exist_ok=True)

    def delete(self, rel):
        self.p(rel).unlink()

    def rmtree(self, rel):
        shutil.rmtree(self.p(rel))

    def rename(self, rel, to):
        self.p(rel).rename(self.p(to))

    def glob(self, rel, pattern):
        return [x.relative_to(self.base).as_posix() for x in self.p(rel).rglob(pattern)]

    def tree(self, rel):
        root = self.p(rel)
        out = {}
        if not root.exists():
            return out
        for x in root.rglob("*"):
            r = x.relative_to(root).as_posix()
            if r.split("/")[0] == ".syncme":
                continue
            if x.is_file():
                out[r] = sha(x.read_bytes())
            elif x.is_dir():
                out[r + "/"] = None
        return out

    def try_state(self):
        try:
            return self.state()
        except Exception:
            return None

    def state(self):
        return self.req("/api/state")


class RemoteNode(LocalNode):
    """A SyncMe instance on another machine, driven through tests/remote_agent.py."""

    def __init__(self, ip, token):
        self.name, self.ip, self.token = "Beta", ip, token
        self.port = 47474
        info = self.agent("info")
        self.base_str, self.sep = info["base"], info["sep"]
        self.id = None

    def agent(self, op, **kw):
        return http_json(f"http://{self.ip}:48600/", dict(op=op, **kw), {"X-Token": self.token})

    def start(self):
        self.agent("start")
        wait(lambda: self.try_state() is not None, "remote SyncMe starts")
        self.id = self.state()["me"]["id"]

    def stop(self):
        self.agent("stop")

    def req(self, path, body=None):
        return self.agent("api", path=path, body=body)

    def abs(self, rel):
        return self.base_str + self.sep + rel.replace("/", self.sep)

    def write(self, rel, content):
        self.agent("write", path=rel, b64=base64.b64encode(content if isinstance(content, bytes) else content.encode()).decode())

    def random(self, rel, size):
        return self.agent("random", path=rel, size=size)["sha256"]

    def read(self, rel):
        return base64.b64decode(self.agent("read", path=rel)["b64"])

    def exists(self, rel):
        return self.agent("exists", path=rel)["exists"]

    def mkdir(self, rel):
        self.agent("mkdir", path=rel)

    def delete(self, rel):
        self.agent("delete", path=rel)

    def rmtree(self, rel):
        self.agent("rmtree", path=rel)

    def rename(self, rel, to):
        self.agent("rename", path=rel, to=to)

    def glob(self, rel, pattern):
        return self.agent("glob", path=rel, pattern=pattern)

    def tree(self, rel):
        return self.agent("tree", path=rel)


def replicas_idle(n):
    st = n.state()
    return all((not r["local"]) or (r["status"] and r["status"]["state"] == "idle")
               for s in st["shares"] for r in s["replicas"]) and not st["transfers"]


def wait(cond, what, timeout=None):
    end = time.time() + (timeout or TIMEOUT)
    last_exc = None
    while time.time() < end:
        try:
            if cond():
                return
        except Exception as e:  # noqa
            last_exc = e
        time.sleep(0.4)
    raise AssertionError(f"timed out waiting for: {what}" + (f" ({last_exc})" if last_exc else ""))


def settle(*nodes, quiet=2.5):
    end = time.time() + TIMEOUT
    stable_since = None
    while time.time() < end:
        if all(replicas_idle(n) for n in nodes):
            stable_since = stable_since or time.time()
            if time.time() - stable_since >= quiet:
                return
        else:
            stable_since = None
        time.sleep(0.3)
    raise AssertionError("nodes did not settle")


def scenario(name):
    def deco(fn):
        def run():
            t0 = time.time()
            try:
                fn()
                results.append((name, True, f"{time.time() - t0:.1f}s"))
                print(f"PASS  {name} ({time.time() - t0:.1f}s)", flush=True)
            except Exception as e:
                results.append((name, False, str(e)))
                print(f"FAIL  {name}: {e}", flush=True)
        return run
    return deco


A = LocalNode("Alpha", 48101)
B = RemoteNode(*REMOTE) if REMOTE else LocalNode("Beta", 48102)
FA, FB = "meow", "meow"   # folder paths relative to each node's base
LA2 = "drive2/meow-copy"  # second local copy on Alpha
SHARE_ID = None


def converged(*pairs):
    trees = [n.tree(rel) for n, rel in pairs]
    return all(t == trees[0] for t in trees[1:])


def pair():
    addr = f"{REMOTE[0]}:47474" if REMOTE else f"127.0.0.1:{B.port}"
    A.req("/api/peers/add", {"address": addr})
    A.req("/api/pair", {"id": B.id})
    wait(lambda: any(i["id"] == A.id for i in B.state()["incoming"]), "pair request arrives")
    B.req("/api/pair/accept", {"id": A.id})
    wait(lambda: any(d["id"] == B.id and d["online"] for d in A.state()["devices"]), "A sees B online")
    wait(lambda: any(d["id"] == A.id and d["online"] for d in B.state()["devices"]), "B sees A online")


@scenario("pre-existing files on both sides are merged, nothing deleted")
def t_initial_merge():
    global SHARE_ID
    A.write(f"{FA}/only-alpha.txt", "A")
    A.write(f"{FA}/same.txt", "identical")
    A.write(f"{FA}/diff.txt", "alpha version")
    A.write(f"{FA}/sub/deep/x.txt", "deep")
    A.write(f"{FA}/canción ñ 日本.txt", "unicode")
    A.mkdir(f"{FA}/empty-dir")
    B.write(f"{FB}/only-beta.txt", "B")
    B.write(f"{FB}/same.txt", "identical")
    B.write(f"{FB}/diff.txt", "beta version")
    share = A.req("/api/shares", {"name": "meow", "replicas": [
        {"node": A.id, "path": A.abs(FA)}, {"node": B.id, "path": B.abs(FB)}]})
    SHARE_ID = share["id"]
    wait(lambda: any(s["id"] == SHARE_ID for s in B.state()["shares"]), "B learns about the folder")
    wait(lambda: converged((A, FA), (B, FB)), "trees converge")
    settle(A, B)
    t = A.tree(FA)
    for f in ["only-alpha.txt", "only-beta.txt", "same.txt", "sub/deep/x.txt", "canción ñ 日本.txt", "empty-dir/"]:
        assert f in t, f"missing {f}"
    versions = sorted([t["diff.txt"]] + [v for k, v in t.items() if "sync-conflict" in k])
    assert versions == sorted([sha(b"alpha version"), sha(b"beta version")]), f"both versions of diff.txt must survive: {t}"
    assert converged((A, FA), (B, FB))


@scenario("edit propagates")
def t_edit():
    A.write(f"{FA}/only-alpha.txt", "A edited")
    wait(lambda: B.read(f"{FB}/only-alpha.txt") == b"A edited", "edit reaches Beta")
    settle(A, B)
    assert converged((A, FA), (B, FB))


@scenario("explicit delete propagates and goes to trash")
def t_delete():
    B.delete(f"{FB}/only-beta.txt")
    wait(lambda: not A.exists(f"{FA}/only-beta.txt"), "delete reaches Alpha")
    settle(A, B)
    assert A.glob(f"{FA}/.syncme/trash", "only-beta.txt"), "deleted file not in Alpha's trash"
    assert converged((A, FA), (B, FB))


@scenario("rename and folder tree delete")
def t_rename():
    A.rename(f"{FA}/same.txt", f"{FA}/renamed.txt")
    wait(lambda: B.exists(f"{FB}/renamed.txt") and not B.exists(f"{FB}/same.txt"), "rename reaches Beta")
    B.rmtree(f"{FB}/sub")
    wait(lambda: not A.exists(f"{FA}/sub"), "folder delete reaches Alpha")
    settle(A, B)
    assert converged((A, FA), (B, FB))


@scenario("concurrent edits keep both versions")
def t_concurrent():
    A.stop()
    B.write(f"{FB}/renamed.txt", "beta edit while alpha offline")
    A.write(f"{FA}/renamed.txt", "alpha edit while offline")
    A.start()
    wait(lambda: converged((A, FA), (B, FB)), "trees converge")
    settle(A, B)
    contents = sorted(v for k, v in A.tree(FA).items() if k.startswith("renamed"))
    assert contents == sorted([sha(b"beta edit while alpha offline"), sha(b"alpha edit while offline")]), contents


@scenario("changes made while a device is offline sync on reconnect")
def t_offline():
    B.stop()
    A.write(f"{FA}/while-beta-off.txt", "hello")
    A.delete(f"{FA}/only-alpha.txt")
    B.start()
    wait(lambda: B.exists(f"{FB}/while-beta-off.txt") and not B.exists(f"{FB}/only-alpha.txt"), "Beta catches up")
    settle(A, B)
    assert converged((A, FA), (B, FB))


def beta_missing():
    return any(r["local"] and r["status"] and r["status"]["state"] == "missing"
               for s in B.state()["shares"] for r in s["replicas"])


@scenario("unplugged/missing folder does not delete anything elsewhere")
def t_missing():
    before = A.tree(FA)
    B.stop()
    B.rename(FB, "meow-moved")
    B.start()
    wait(beta_missing, "Beta reports missing folder")
    time.sleep(3)
    assert A.tree(FA) == before, "Alpha lost files while Beta's folder was missing"
    B.stop()
    B.rename("meow-moved", FB)
    B.start()
    settle(A, B)
    assert A.tree(FA) == before and converged((A, FA), (B, FB))


@scenario("recreated empty folder with same path is not treated as mass delete")
def t_recreated():
    before = A.tree(FA)
    B.stop()
    B.rmtree(FB)
    B.mkdir(FB)
    B.start()
    wait(beta_missing, "Beta refuses to sync the recreated folder")
    time.sleep(3)
    assert A.tree(FA) == before, "Alpha lost files"
    rep = next(r for s in B.state()["shares"] for r in s["replicas"] if r["local"])
    B.req(f"/api/replicas/{rep['id']}/reset", {})
    wait(lambda: converged((A, FA), (B, FB)), "reset folder refills from Alpha")
    settle(A, B)
    assert A.tree(FA) == before


@scenario("large file transfer (64 MB)")
def t_large():
    h = A.random(f"{FA}/big.bin", 64 * 1024 * 1024)
    wait(lambda: B.tree(FB).get("big.bin") == h, "big file arrives intact", timeout=300)
    settle(A, B)


@scenario("burst of 300 small files")
def t_many():
    for i in range(300):
        B.write(f"{FB}/many/f{i:03}.txt", f"file {i}")
    wait(lambda: len([k for k in A.tree(FA) if k.startswith("many/f")]) == 300, "all files arrive", timeout=180)
    settle(A, B)
    assert converged((A, FA), (B, FB))


@scenario("local-to-local: second folder on the same computer joins")
def t_local():
    A.write(f"{LA2}/preexisting-on-drive2.txt", "d2")
    share = next(s for s in A.state()["shares"] if s["id"] == SHARE_ID)
    reps = [{"id": r["id"], "node": r["node"], "path": r["path"]} for r in share["replicas"]]
    reps.append({"node": A.id, "path": A.abs(LA2)})
    A.req("/api/shares", {"id": SHARE_ID, "name": "meow", "replicas": reps})
    wait(lambda: converged((A, FA), (B, FB), (A, LA2)), "three copies converge", timeout=180)
    settle(A, B)
    assert "preexisting-on-drive2.txt" in B.tree(FB)
    A.write(f"{LA2}/from-drive2.txt", "x")
    wait(lambda: B.exists(f"{FB}/from-drive2.txt") and A.exists(f"{FA}/from-drive2.txt"), "drive2 change spreads")
    B.delete(f"{FB}/from-drive2.txt")
    wait(lambda: not A.exists(f"{LA2}/from-drive2.txt"), "delete reaches drive2")
    settle(A, B)
    assert converged((A, FA), (B, FB), (A, LA2))


@scenario("removing a device from a folder keeps its files")
def t_remove_member():
    share = next(s for s in A.state()["shares"] if s["id"] == SHARE_ID)
    reps = [{"id": r["id"], "node": r["node"], "path": r["path"]} for r in share["replicas"] if r["node"] != B.id]
    before = B.tree(FB)
    A.req("/api/shares", {"id": SHARE_ID, "name": "meow", "replicas": reps})
    wait(lambda: not any(s["id"] == SHARE_ID for s in B.state()["shares"]), "Beta drops the folder")
    A.write(f"{FA}/after-removal.txt", "not for beta")
    time.sleep(3)
    assert B.tree(FB) == before, "Beta's files changed after removal"


def main():
    print(f"binary: {BIN}\nwork dir: {ROOT}\nBeta: {'remote ' + REMOTE[0] if REMOTE else 'local'}", flush=True)
    A.start()
    B.start()
    try:
        pair()
        for t in [t_initial_merge, t_edit, t_delete, t_rename, t_concurrent, t_offline, t_missing,
                  t_recreated, t_large, t_many, t_local, t_remove_member]:
            t()
    finally:
        A.stop()
        B.stop()
    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} scenarios passed", flush=True)
    if failed:
        print(f"logs kept in {ROOT}")
        sys.exit(1)
    shutil.rmtree(ROOT, ignore_errors=True)


if __name__ == "__main__":
    main()
