#!/usr/bin/env python3
"""concord-team: see what your teammates' AIs are changing, while they change it.

Everyone keeps their own AI tool (Claude Code, Cursor, OpenCode, ...) and their
own clone. concord-team snapshots each person's work in progress, shares it
with the team directly over Tailscale (and through an always-online relay), and
runs Concord's contract and overlap analysis locally against every teammate's
work and against main. Clashes come with a verdict: who adapts, and why.

  concord-team init                 start a team in this repo, print an invite
  concord-team join <invite>        join a team in your clone of the repo
  concord-team up                   live team view (keep it running)
  concord-team status               one-shot overview
  concord-team say <name|team> msg  message a teammate (or their AI)
  concord-team urgent on|off        mark your work urgent (you win clashes)
  concord-team setup claude         notices, team brief and status line in Claude Code
  concord-team doctor               check git, concord, Tailscale, peers
  concord-team relay <url> <key>    also sync through an always-online relay
  concord-team serve                run the relay + Discord bot (on a server)
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import fcntl
import hashlib
import hmac
import http.server
import json
import os
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

VERSION = "0.2.0"
PORTS = range(7421, 7426)
SNAP = "refs/concord/snap"
PEERS = "refs/concord/peers/"
PEER_POLL = 10  # seconds between peer state polls when nothing pokes us
MAILBOX_POLL = 30
DISCOVER_EVERY = 45
FETCH_EVERY = 60
ONLINE_FOR = 35  # a peer answered within this many seconds = online

ROOT = ""
WAKE = threading.Event()

# ------------------------------------------------------------------ output

COLOR = sys.stdout.isatty() and not os.environ.get("NO_COLOR")


def paint(code: str, s: str, on: bool | None = None) -> str:
    return f"\x1b[{code}m{s}\x1b[0m" if (COLOR if on is None else on) else s


def bold(s): return paint("1", s)
def dim(s): return paint("2", s)
def red(s): return paint("31", s)
def green(s): return paint("32", s)
def yellow(s): return paint("33", s)
def cyan(s): return paint("36", s)


def ms() -> int:
    return int(time.time() * 1000)


def hm(t: int | None) -> str:
    return time.strftime("%H:%M:%S", time.localtime(t / 1000)) if t else "?"


def ago(t: int | None) -> str:
    if not t:
        return "never"
    s = max(0, (ms() - t) // 1000)
    return f"{s}s ago" if s < 90 else f"{s // 60}m ago" if s < 5400 else f"{s // 3600}h ago"


def slug(s: str, n: int = 30) -> str:
    s = re.sub(r"[^a-z0-9_-]+", "-", s.lower()).strip("-_")
    return (s or "x")[:n]


# --------------------------------------------------------------------- git


def git(*args, env=None, inp=None, check=True, timeout=120) -> str:
    e = dict(os.environ)
    if env:
        e.update(env)
    r = subprocess.run(["git", *args], cwd=ROOT or None, env=e, input=inp, capture_output=True, timeout=timeout)
    if check and r.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {r.stderr.decode(errors='replace').strip()}")
    return r.stdout.decode(errors="replace").strip()


def git_ok(*args) -> bool:
    return subprocess.run(["git", *args], cwd=ROOT or None, capture_output=True).returncode == 0


def has_commit(sha: str | None) -> bool:
    return bool(sha) and git_ok("cat-file", "-e", f"{sha}^{{commit}}")


def find_root(start: str | None = None) -> str:
    try:
        r = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=start or None, capture_output=True, text=True)
    except OSError:  # no git (e.g. the relay's container) or a missing directory
        return ""
    return r.stdout.strip() if r.returncode == 0 else ""


def merge_base(a: str, b: str) -> str:
    return git("merge-base", a, b, check=False)


# ----------------------------------------------------------------- storage


def tdir() -> str:
    return os.path.join(ROOT, ".concord", "team")


def load(name: str, default):
    try:
        with open(os.path.join(tdir(), name)) as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


def save(name: str, data) -> None:
    os.makedirs(tdir(), exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=tdir(), prefix=f".{name}.")
    with os.fdopen(fd, "w") as f:
        json.dump(data, f, indent=1)
    os.replace(tmp, os.path.join(tdir(), name))


@contextlib.contextmanager
def locked(name: str = ".lock", block: bool = True):
    os.makedirs(tdir(), exist_ok=True)
    with open(os.path.join(tdir(), name), "a") as f:
        try:
            fcntl.flock(f, fcntl.LOCK_EX | (0 if block else fcntl.LOCK_NB))
        except BlockingIOError:
            yield False
            return
        try:
            yield True
        finally:
            fcntl.flock(f, fcntl.LOCK_UN)


def config(required: bool = True) -> dict | None:
    c = load("team.json", None)
    if not c and required:
        sys.exit("not in a team: run `concord-team init` (new team) or `concord-team join <invite>` in your clone")
    return c


def push_feed(text: str) -> None:
    with locked():
        feed = load("feed.json", [])
        feed.append({"t": ms(), "text": text})
        save("feed.json", feed[-50:])


def push_inbox(items: list[dict]) -> None:
    if not items:
        return
    with locked():
        inbox = load("inbox.json", [])
        inbox.extend(items)
        save("inbox.json", inbox[-100:])


# ------------------------------------------------------------------- tools


def concord_bin(c: dict) -> str:
    for p in (c.get("concord"), shutil.which("concord"), os.path.expanduser("~/.local/bin/concord"),
              "/opt/homebrew/bin/concord", "/usr/local/bin/concord"):
        if p and os.path.isfile(p) and os.access(p, os.X_OK):
            return p
    sys.exit("concord not found: brew install lucas-rus/tap/concord")


def tailscale_bin() -> str | None:
    for p in (shutil.which("tailscale"), "/Applications/Tailscale.app/Contents/MacOS/Tailscale"):
        if p and os.path.exists(p):
            return p
    return None


def tailscale_status() -> dict | None:
    ts = tailscale_bin()
    if not ts:
        return None
    try:
        r = subprocess.run([ts, "status", "--json"], capture_output=True, text=True, timeout=5)
        st = json.loads(r.stdout) if r.returncode == 0 else None
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return None
    return st if st and st.get("BackendState") == "Running" else None


def tailscale_ip(st: dict | None) -> str | None:
    if not st:
        return None
    for ip in (st.get("Self") or {}).get("TailscaleIPs") or []:
        if "." in ip:
            return ip
    return None


# ------------------------------------------------------------------ invite


def make_invite(c: dict) -> str:
    d = {"p": c["project"], "t": c["token"], "main": c["main"]}
    if c.get("mailbox"):
        d["m"] = c["mailbox"]
    if c.get("discord"):
        d["d"] = c["discord"]
    raw = base64.urlsafe_b64encode(json.dumps(d, separators=(",", ":")).encode()).decode().rstrip("=")
    return "ct1_" + raw


def read_invite(s: str) -> dict:
    s = s.strip()
    if not s.startswith("ct1_"):
        sys.exit("that is not a concord-team invite (it should start with ct1_)")
    raw = s[4:] + "=" * (-len(s[4:]) % 4)
    try:
        return json.loads(base64.urlsafe_b64decode(raw))
    except ValueError:
        sys.exit("the invite is damaged: copy it again in one piece")


# ---------------------------------------------------------------- snapshot


def snapshot() -> tuple[str, bool]:
    """Commit of the working tree (tracked + untracked, not ignored) on top of HEAD.

    Starts from a copy of the real index so git only rehashes changed files."""
    fd, idx = tempfile.mkstemp(prefix="concord-team-index-")
    os.close(fd)
    env = {"GIT_INDEX_FILE": idx}
    try:
        src = git("rev-parse", "--git-path", "index")
        src = src if os.path.isabs(src) else os.path.join(ROOT, src)
        try:
            shutil.copyfile(src, idx)
            git("add", "-A", "--", ".", env=env)
            tree = git("write-tree", env=env)
        except (OSError, RuntimeError):  # no index yet, or a merge in progress
            os.unlink(idx)
            git("read-tree", "HEAD", env=env)
            git("add", "-A", "--", ".", env=env)
            tree = git("write-tree", env=env)
    finally:
        if os.path.exists(idx):
            os.unlink(idx)
    head = git("rev-parse", "HEAD")
    st = load("state.json", {})
    if st.get("tree") == tree and st.get("head") == head and has_commit(st.get("snap")):
        return st["snap"], False
    ident = {"GIT_AUTHOR_NAME": "concord-team", "GIT_AUTHOR_EMAIL": "concord@localhost",
             "GIT_COMMITTER_NAME": "concord-team", "GIT_COMMITTER_EMAIL": "concord@localhost"}
    snap = git("commit-tree", tree, "-p", head, "-m", "concord-team snapshot", env=ident)
    with locked():
        st = load("state.json", {})
        st.update(tree=tree, head=head, snap=snap)
        save("state.json", st)
    return snap, True


def analyze(c: dict, base: str, a: str, b: str, textual: bool = True) -> dict:
    cmd = [concord_bin(c), "analyze", "--repo", ROOT, "--base", base, a, b, "--json"]
    if not textual:
        cmd.append("--no-textual")
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
    if r.returncode != 0:
        raise RuntimeError(f"concord analyze: {r.stderr.strip()[:300]}")
    return json.loads(r.stdout)["analysis"]


def refresh_self(c: dict) -> bool:
    """Snapshot my work; rebuild my published meta if anything changed."""
    snap, _ = snapshot()
    git("update-ref", SNAP, snap)
    st = load("state.json", {})
    meta = st.get("meta") or {}
    if meta.get("sha") == snap and meta.get("urgent") == bool(c.get("urgent")) and meta.get("tool") == c.get("tool", ""):
        return False
    base = merge_base(snap, c["main"]) or git("rev-parse", "HEAD")
    files, symbols = [], []
    if base != snap:
        a = analyze(c, base, snap, base, textual=False)
        mine = a["branches"][0]
        files = mine.get("changed_files", [])
        symbols = [ch["symbol"] for ch in mine.get("changes", [])]
    now = ms()
    # First time each symbol/file entered my work: the basis of "who was first".
    sym_t = {s: (st.get("symbols") or {}).get(s, now) for s in symbols}
    file_t = {f: (st.get("files") or {}).get(f, now) for f in files}
    path = os.path.join(tdir(), "snap.bundle")
    git("bundle", "create", path + ".tmp", SNAP, f"^{base}") if base != snap else None
    if os.path.exists(path + ".tmp"):
        os.replace(path + ".tmp", path)
    meta = {
        "user": c["name"], "sha": snap, "base": base,
        "branch": git("rev-parse", "--abbrev-ref", "HEAD", check=False),
        "files": file_t, "symbols": sym_t, "tool": c.get("tool", ""),
        "urgent": bool(c.get("urgent")), "at": now, "version": VERSION,
    }
    with locked():
        st = load("state.json", {})
        st.update(meta=meta, symbols=sym_t, files=file_t)
        save("state.json", st)
    return True


def my_meta() -> dict:
    return load("state.json", {}).get("meta") or {}


def my_bundle() -> bytes | None:
    try:
        with open(os.path.join(tdir(), "snap.bundle"), "rb") as f:
            return f.read()
    except OSError:
        return None


# --------------------------------------------------------------- transports


def call(method: str, url: str, token: str, body: bytes | None = None, headers=None, timeout=10, raw=False):
    h = {"authorization": f"Bearer {token}", "user-agent": f"concord-team/{VERSION}", **(headers or {})}
    req = urllib.request.Request(url, data=body, method=method, headers=h)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        data = r.read()
    return data if raw else json.loads(data or b"{}")


class Mailbox:
    """Optional store-and-forward relay (Vercel), for teammates who are not online together."""

    name = "mailbox"

    def __init__(self, c: dict):
        self.c = c
        self.url = c["mailbox"]["url"].rstrip("/")
        self.key = c["mailbox"]["key"]
        self.project = f"{c['project'][:30]}-{hashlib.sha256(c['token'].encode()).hexdigest()[:8]}"
        self.ok: bool | None = None
        self.error = ""

    def req(self, method, path, query=None, **kw):
        q = urllib.parse.urlencode({"project": self.project, **(query or {})})
        try:
            out = call(method, f"{self.url}{path}?{q}", self.key, **kw)
            self.ok = True
            return out
        except (urllib.error.URLError, OSError, ValueError) as e:
            self.ok, self.error = False, str(e)[:120]
            raise

    def publish(self, meta: dict, bundle: bytes) -> None:
        self.req("POST", "/api/snapshot", {"user": meta["user"]}, body=bundle, timeout=30,
                 headers={"x-concord-meta": base64.b64encode(json.dumps(meta).encode()).decode(),
                          "content-type": "application/octet-stream"})

    def states(self) -> dict:
        users = self.req("GET", "/api/state").get("users", {})
        return {u: m for u, m in users.items() if u != self.c["name"]}

    def bundle(self, user: str, sha: str) -> bytes:
        return self.req("GET", "/api/snapshot", {"user": user, "sha": sha}, raw=True, timeout=30)

    def send(self, msg: dict) -> bool:
        self.req("POST", "/api/message", body=json.dumps(msg).encode(), headers={"content-type": "application/json"})
        return True

    def messages(self, since: int) -> list[dict]:
        return self.req("GET", "/api/message", {"user": self.c["name"], "since": since}).get("messages", [])


class Peers:
    """Direct peer-to-peer sync with teammates over Tailscale (and on this machine)."""

    name = "tailnet"

    def __init__(self, c: dict, my_port: int | None = None):
        self.c = c
        self.my_port = my_port
        self.ts = None

    def known(self) -> dict:
        return load("peers.json", {})

    def _set(self, user: str, **kw) -> None:
        with locked():
            k = load("peers.json", {})
            k.setdefault(user, {}).update(kw)
            save("peers.json", k)

    def online(self) -> dict:
        now = ms()
        return {u: i for u, i in self.known().items() if now - i.get("last_ok", 0) < ONLINE_FOR * 1000}

    def url(self, addr: str, path: str, query=None) -> str:
        q = urllib.parse.urlencode({"project": self.c["project"], **(query or {})})
        return f"http://{addr}{path}?{q}"

    def hello(self, addr: str):
        try:
            h = call("GET", self.url(addr, "/api/hello"), self.c["token"], timeout=1.2)
        except (urllib.error.URLError, OSError, ValueError):
            return None
        if h.get("app") == "concord-team" and h.get("project") == self.c["project"] and h.get("user") != self.c["name"]:
            return h
        return None

    def discover(self) -> list[str]:
        """Probe teammates: Tailscale peers that are online, plus other clones on this machine."""
        self.ts = tailscale_status()
        hosts = []
        for p in ((self.ts or {}).get("Peer") or {}).values():
            if p.get("Online"):
                ip = next((x for x in p.get("TailscaleIPs") or [] if "." in x), None)
                if ip:
                    hosts.append(ip)
        addrs = [f"{h}:{p}" for h in hosts for p in PORTS]
        addrs += [f"127.0.0.1:{p}" for p in PORTS if p != self.my_port]
        addrs += [a for a in self.c.get("peers_static", [])]
        addrs += [i["addr"] for i in self.known().values() if i.get("addr")]
        found: dict[str, str] = {}
        lock = threading.Lock()

        def probe(a):
            h = self.hello(a)
            if h:
                with lock:
                    # prefer a tailnet address over localhost for the same person
                    if h["user"] not in found or found[h["user"]].startswith("127."):
                        found[h["user"]] = a

        threads = [threading.Thread(target=probe, args=(a,), daemon=True) for a in dict.fromkeys(addrs)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(2)
        for user, addr in found.items():
            new = user not in self.online()
            self._set(user, addr=addr, last_ok=ms())
            if new:
                push_feed(f"● {user} is online ({'this machine' if addr.startswith('127.') else 'tailnet'})")
        return list(found)

    def states(self) -> dict:
        out = {}
        for user, info in self.known().items():
            meta = info.get("meta")
            if info.get("addr"):
                try:
                    st = call("GET", self.url(info["addr"], "/api/state"), self.c["token"], timeout=3)
                    m = (st.get("users") or {}).get(user)
                    if m:
                        meta = m
                        self._set(user, meta=m, last_ok=ms())
                except (urllib.error.URLError, OSError, ValueError):
                    pass
            if meta:
                out[user] = meta
        return out

    def bundle(self, user: str, sha: str) -> bytes:
        addr = self.known()[user]["addr"]
        return call("GET", self.url(addr, "/api/snapshot", {"user": user, "sha": sha}), self.c["token"], raw=True, timeout=30)

    def poke_all(self) -> None:
        for user, info in self.online().items():
            threading.Thread(target=self._poke, args=(info["addr"],), daemon=True).start()

    def _poke(self, addr: str) -> None:
        with contextlib.suppress(Exception):
            call("POST", self.url(addr, "/api/poke"), self.c["token"], body=b"{}", timeout=2)

    def send(self, msg: dict) -> bool:
        targets = self.online() if msg["to"] == "team" else {k: v for k, v in self.online().items() if k == msg["to"]}
        ok = False
        for user, info in targets.items():
            try:
                call("POST", self.url(info["addr"], "/api/message"), self.c["token"], body=json.dumps(msg).encode(),
                     headers={"content-type": "application/json"}, timeout=4)
                ok = True
            except (urllib.error.URLError, OSError, ValueError):
                pass
        return ok


# -------------------------------------------------------------- peer server


class Handler(http.server.BaseHTTPRequestHandler):
    server_version = f"concord-team/{VERSION}"
    cfg: dict = {}

    def log_message(self, *a):
        pass

    def _send(self, code: int, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _authed(self) -> bool:
        got = (self.headers.get("authorization") or "").removeprefix("Bearer ").strip()
        q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        ok = hmac.compare_digest(got.encode(), self.cfg["token"].encode()) and \
            q.get("project", [""])[0] == self.cfg["project"]
        if not ok:
            self._send(401, {"error": "unauthorized"})
        return ok

    def do_GET(self):
        if not self._authed():
            return
        u = urllib.parse.urlparse(self.path)
        q = urllib.parse.parse_qs(u.query)
        me = self.cfg["name"]
        if u.path == "/api/hello":
            return self._send(200, {"app": "concord-team", "project": self.cfg["project"], "user": me, "version": VERSION})
        if u.path == "/api/state":
            m = my_meta()
            return self._send(200, {"users": {me: m} if m else {}, "now": ms()})
        if u.path == "/api/snapshot":
            b = my_bundle()
            if q.get("user", [""])[0] != me or q.get("sha", [""])[0] != my_meta().get("sha") or not b:
                return self._send(404, {"error": "no such snapshot"})
            return self._send(200, b, "application/octet-stream")
        self._send(404, {"error": "not found"})

    def do_POST(self):
        if not self._authed():
            return
        n = int(self.headers.get("content-length") or 0)
        if n > 65536:
            return self._send(413, {"error": "too large"})
        body = self.rfile.read(n)
        path = urllib.parse.urlparse(self.path).path
        if path == "/api/poke":
            WAKE.set()
            return self._send(200, {"ok": True})
        if path == "/api/message":
            try:
                m = json.loads(body)
                msg = {"from": slug(str(m["from"])), "to": str(m["to"])[:40], "text": str(m["text"])[:4000],
                       "kind": str(m.get("kind", "note"))[:20], "at": ms()}
            except (ValueError, KeyError):
                return self._send(400, {"error": "bad message"})
            receive_messages([msg])
            WAKE.set()
            return self._send(200, {"ok": True})
        self._send(404, {"error": "not found"})


def start_server(c: dict, ts_ip: str | None) -> int:
    Handler.cfg = c
    for port in PORTS:
        try:
            servers = [http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)]
        except OSError:
            continue
        if ts_ip:
            with contextlib.suppress(OSError):
                servers.append(http.server.ThreadingHTTPServer((ts_ip, port), Handler))
        for s in servers:
            s.daemon_threads = True
            threading.Thread(target=s.serve_forever, daemon=True).start()
        return port
    sys.exit(f"ports {PORTS.start}-{PORTS.stop - 1} are all busy")


# -------------------------------------------------------------- the verdict


def owner(a: str, b: str, ta, tb, metas: dict) -> tuple[str, str]:
    """Who keeps their version in a clash between `a` and `b`, and why.

    1. main always wins: merged work is final.
    2. urgent work wins over normal work (set openly with `concord-team urgent on`).
    3. otherwise, whoever got there first: the later change adapts (the earlier
       one could not have known; the later one was told the moment it clashed).
    4. a tie is broken by name, so both laptops always reach the same verdict."""
    if "main" in (a, b):
        return "main", "it is already merged in main"
    ua, ub = bool(metas.get(a, {}).get("urgent")), bool(metas.get(b, {}).get("urgent"))
    if ua != ub:
        w = a if ua else b
        return w, f"{w} marked their work urgent"
    if ta and tb and ta != tb:
        w, l, tw, tl = (a, b, ta, tb) if ta < tb else (b, a, tb, ta)
        return w, f"{w} got there first ({hm(tw)} vs {l} at {hm(tl)})"
    w = min(a, b)
    return w, "same time, decided by name"


def site_text(f: dict) -> str:
    v = f.get("violations") or []
    if v:
        return "; ".join(s.strip() for s in v[:3])
    calls = [s for s in f.get("sites", []) if s.get("call")] or f.get("sites", [])
    return "; ".join(f"{s['path']}:{s['line']}  {s['text'].strip()}" for s in calls[:3])


def short(sym: str) -> str:
    return sym.rsplit(".", 1)[-1]


def findings_against(c: dict, peer: str, ref: str, metas: dict) -> list[dict]:
    me = c["name"]
    base = merge_base(SNAP, ref)
    if not base:
        return []
    a = analyze(c, base, SNAP, ref)
    who = lambda r: me if r == SNAP else peer
    out = []
    clash_files = set()
    for tx in a.get("textual", []):
        if not tx.get("clean"):
            files = tx.get("conflicted_files", [])
            clash_files.update(files)
            mf = metas[me].get("files") or {}
            t_me = min([mf.get(f) for f in files if mf.get(f)] or [None])
            t_peer = min([(metas.get(peer, {}).get("files") or {}).get(f) for f in files
                          if (metas.get(peer, {}).get("files") or {}).get(f)] or [None])
            w, why = owner(me, peer, t_me, t_peer, metas)
            y = peer if w == me else me
            fl = ", ".join(files)
            out.append({
                "key": f"text|{min(me, peer)}|{max(me, peer)}|{fl}", "icon": "✖", "you_fix": y == me, "peer": peer,
                "head": f"you and {peer} edited the same lines in {fl}",
                "detail": "merging your branches will conflict here",
                "verdict": (f"→ you yield: move your change elsewhere, or wait and build on {peer}'s · {why}" if y == me
                            else f"→ {peer} yields · {why}"),
                "ai": (f"Leave these lines in {fl} alone or move your change at least one unchanged line away; "
                       f"once {peer}'s work is in, build on their version." if y == me else
                       f"{peer} is expected to move their change; keep going."),
                "team": f"✖ {me} and {peer} edited the same lines in {fl} → {y} yields ({why})",
                "poster": min(me, peer), "fixer": y, "parties": [me, peer],
            })
    for f in a.get("findings", []):
        t = f.get("type")
        if t == "contract_dependency":
            P, C = who(f["producer"]), who(f["consumer"])
            breaking = bool(f.get("breaking"))
            checked = any(s.get("call") for s in f.get("sites", []))
            if breaking and checked and not f.get("violations"):
                continue  # every call was checked against the new signature and fits
            if not breaking and C != me:
                continue
            before = (f.get("before") or {}).get("display", "?")
            after = (f.get("after") or {}).get("display", "(removed)")
            why_c = ", ".join(f.get("reasons") or []) or "changed"
            site = next((s for s in f.get("sites", []) if s.get("call")), (f.get("sites") or [{}])[0])
            tP = (metas.get(P, {}).get("symbols") or {}).get(f["symbol"])
            tC = (metas.get(C, {}).get("symbols") or {}).get(site.get("from", "")) or \
                (metas.get(C, {}).get("files") or {}).get(site.get("path", ""))
            w, why = owner(P, C, tP, tC, metas)
            fixer = C if w == P else P
            m = re.search(r"new required parameter `(\w+)`", why_c)
            compat = f" — e.g. give `{m.group(1)}` a default so old calls still work" if m else ""
            key = f"contract|{P}|{C}|{f['symbol']}|{(f.get('after') or {}).get('digest', '')}"
            if not breaking:
                out.append({"key": key, "icon": "ℹ", "you_fix": False, "peer": peer,
                            "head": f"{P} changed `{before}` → `{after}` (compatible)",
                            "detail": f"you use it: {site_text(f)}", "verdict": "→ nothing to do",
                            "ai": "No action needed.", "team": "", "poster": ""})
                continue
            if C == me:
                head = f"{P} changed `{before}` → `{after}` ({why_c})"
                detail = f"your code: {site_text(f)}"
            else:
                head = f"your change `{after}` ({why_c}) breaks {C}'s code"
                detail = f"{C}'s code: {site_text(f)}"
            if fixer == me and C == me:
                verdict, ai = f"→ you adapt · {why}", "Update your call sites to the new signature now."
            elif fixer == me:
                verdict = f"→ you fix: keep it compatible{compat}, or agree it with {C} · {why}"
                ai = (f"Your change breaks {C}'s work in progress, and {C} was first. Prefer a compatible change{compat}; "
                      f"if it must break, tell them: concord-team say {C} \"...\"")
            else:
                other = P if C == me else C
                verdict = f"→ {other} {'fixes' if other == P else 'adapts'} · {why}"
                ai = (f"{other} is expected to handle this; don't work around it. If you are blocked, "
                      f"message them: concord-team say {other} \"...\"")
            out.append({"key": key, "icon": "⚠", "you_fix": fixer == me, "peer": peer, "head": head,
                        "detail": detail, "verdict": verdict, "ai": ai,
                        "team": f"⚠ {P} changed `{after}` ({why_c}); it breaks {C}'s {site.get('path', '?')}:"
                                f"{site.get('line', '?')} → {fixer} fixes ({why})",
                        "poster": C, "fixer": fixer, "parties": [P, C]})
        elif t == "duplicate_definition":
            sym = f.get("symbol", "?")
            w, why = owner(me, peer, (metas[me].get("symbols") or {}).get(sym),
                           (metas.get(peer, {}).get("symbols") or {}).get(sym), metas)
            y = peer if w == me else me
            out.append({"key": f"dup|{min(me, peer)}|{max(me, peer)}|{sym}", "icon": "⚠", "you_fix": y == me,
                        "peer": peer, "head": f"you and {peer} both added `{sym}`",
                        "detail": "two definitions of the same thing",
                        "verdict": f"→ {'you reuse theirs' if y == me else peer + ' reuses yours'} · {why}",
                        "ai": f"Reuse {peer}'s `{short(sym)}` instead of defining your own." if y == me else "Keep yours.",
                        "team": f"⚠ {me} and {peer} both added `{sym}` → {y} reuses the other's ({why})",
                        "poster": min(me, peer), "fixer": y, "parties": [me, peer]})
        elif t == "same_symbol" and not clash_files:
            sym = f.get("symbol", "?")
            out.append({"key": f"same|{min(me, peer)}|{max(me, peer)}|{sym}", "icon": "✎", "you_fix": False,
                        "peer": peer, "head": f"you and {peer} are both changing `{sym}`",
                        "detail": "different lines so far, merges cleanly", "verdict": "→ just be aware",
                        "ai": f"{peer} is also editing `{short(sym)}`; keep your change focused.", "team": "",
                        "poster": ""})
    return out


# -------------------------------------------------------------------- sync


class Sync:
    """One round: publish my work, gather teammates', analyse, deliver."""

    def __init__(self, c: dict, port: int | None = None):
        self.c = c
        self.peers = Peers(c, port)
        self.mailbox = Mailbox(c) if c.get("mailbox") else None
        self.cache: dict[str, list] = {}
        self.last = {"discover": 0.0, "mailbox": 0.0, "fetch": 0.0, "poll": 0.0}

    def publish(self) -> bool:
        changed = refresh_self(self.c)
        if changed:
            self.peers.poke_all()
        meta = my_meta()
        if self.mailbox and my_bundle() and load("state.json", {}).get("mb_sha") != meta.get("sha") + self.mailbox.url:
            with contextlib.suppress(Exception):
                self.mailbox.publish(meta, my_bundle())
                with locked():
                    st = load("state.json", {})
                    st["mb_sha"] = meta.get("sha") + self.mailbox.url
                    save("state.json", st)
        return changed

    def gather(self, use_mailbox: bool) -> dict:
        metas: dict[str, tuple[dict, object]] = {}
        sources = [self.peers] + ([self.mailbox] if use_mailbox and self.mailbox else [])
        for src in sources:
            try:
                for u, m in src.states().items():
                    if u not in metas or m.get("at", 0) > metas[u][0].get("at", 0):
                        metas[u] = (m, src)
            except Exception:
                pass
        team = load("team_cache.json", {})
        online = self.peers.online()
        for u, (m, src) in metas.items():
            if not has_commit(m["sha"]):
                try:
                    data = src.bundle(u, m["sha"])
                    with tempfile.TemporaryDirectory() as d:
                        p = os.path.join(d, "peer.bundle")
                        with open(p, "wb") as f:
                            f.write(data)
                        git("fetch", "-q", p, f"+{SNAP}:{PEERS}{u}")
                except Exception as e:
                    push_feed(f"! could not read {u}'s work ({str(e)[:80]}); try `git fetch origin`")
                    continue
            git("update-ref", PEERS + u, m["sha"])
            team[u] = {"meta": m, "via": src.name}
        for u in team:
            relay_seen = team[u]["meta"].get("seen") or 0
            team[u]["online"] = u in online or ms() - relay_seen < 60_000
            team[u]["seen"] = max(online.get(u, {}).get("last_ok", 0), relay_seen) or team[u].get("seen")
        save("team_cache.json", team)
        return team

    def analyse(self, team: dict) -> list[dict]:
        me = self.c["name"]
        metas = {me: my_meta()} | {u: t["meta"] for u, t in team.items()}
        found: list[dict] = []
        my_sha = metas[me].get("sha")
        for u, t in team.items():
            if not has_commit(t["meta"]["sha"]):
                continue
            k = f"{my_sha}|{t['meta']['sha']}|{metas[me].get('urgent')}|{t['meta'].get('urgent')}"
            if k not in self.cache:
                try:
                    self.cache[k] = findings_against(self.c, u, PEERS + u, metas)
                except Exception as e:
                    push_feed(f"! analysis against {u} failed: {str(e)[:100]}")
                    self.cache[k] = []
            found += self.cache[k]
        main = self.c["main"]
        if git_ok("rev-parse", "--verify", "-q", main) and not git_ok("merge-base", "--is-ancestor", main, SNAP):
            msha = git("rev-parse", main)
            k = f"{my_sha}|main:{msha}"
            if k not in self.cache:
                try:
                    self.cache[k] = findings_against(self.c, "main", main, metas | {"main": {}})
                except Exception:
                    self.cache[k] = []
            found += self.cache[k]
        return found

    def deliver(self, found: list[dict]) -> None:
        with locked():
            seen = load("seen.json", {})
            now = {f["key"]: f for f in found}
            new = [f for k, f in now.items() if k not in seen]
            gone = [f for k, f in seen.items() if k not in now]
            flipped = [f for k, f in now.items() if k in seen and f.get("you_fix") != seen[k].get("you_fix")]
            save("seen.json", now)
        items = []
        for f in new:
            push_feed(f"{f['icon']} {f['head']}  {f['verdict']}")
            items.append({"key": f["key"], "kind": "new",
                          "text": f"{f['icon']} {f['head']}\n   {f['detail']}\n   {f['verdict']}\n   What to do: {f['ai']}"})
            if f.get("team") and f.get("poster") == self.c["name"]:
                self.post_team(f)
        for f in flipped:
            push_feed(f"↺ {f['head']}  now {f['verdict']}")
            items.append({"key": f["key"] + "|flip", "kind": "msg",
                          "text": f"↺ the verdict changed: {f['head']}\n   now {f['verdict']}\n   What to do: {f['ai']}"})
        for f in gone:
            push_feed(f"✓ resolved: {f['head']}")
            if f.get("team") and f.get("poster") == self.c["name"]:
                self.post_team(f, resolved=True)
            items.append({"key": f["key"], "kind": "resolved", "text": f"✓ resolved: {f['head']}"})
        push_inbox(items)

    def post_team(self, f: dict, resolved: bool = False) -> None:
        text = ("✓ resolved: " + f["team"].lstrip("⚠✖ ")) if resolved else f["team"]
        if self.mailbox:
            with contextlib.suppress(Exception):
                self.mailbox.send({"from": self.c["name"], "to": "team", "text": text,
                                   "kind": "resolved" if resolved else "finding", "key": f["key"],
                                   "fixer": f.get("fixer", ""), "parties": f.get("parties", [])})
        if self.c.get("discord"):
            with contextlib.suppress(Exception):
                body = json.dumps({"content": f"**concord** · {text}"[:1900], "allowed_mentions": {"parse": []}}).encode()
                req = urllib.request.Request(self.c["discord"], data=body, method="POST",
                                             headers={"content-type": "application/json",
                                                      "user-agent": f"concord-team/{VERSION}"})
                urllib.request.urlopen(req, timeout=8).read()

    def mailbox_messages(self) -> None:
        if not self.mailbox:
            return
        st = load("state.json", {})
        try:
            cursor = f"msg_since:{self.mailbox.url}"
            msgs = self.mailbox.messages(st.get(cursor, 0))
        except Exception:
            return
        if msgs:
            receive_messages([m for m in msgs if m.get("from") != self.c["name"] and m.get("kind") not in ("finding", "resolved")])
            with locked():
                st = load("state.json", {})
                st[cursor] = max([st.get(cursor, 0)] + [m["at"] for m in msgs])
                save("state.json", st)

    def round(self, force_pull: bool = False) -> None:
        t = time.time()
        changed = self.publish()
        if t - self.last["discover"] > (DISCOVER_EVERY if self.peers.online() else 10):
            self.peers.discover()
            self.last["discover"] = t
        use_mb = bool(self.mailbox) and t - self.last["mailbox"] > self.c["mailbox"].get("poll", MAILBOX_POLL)
        if changed or force_pull or use_mb or t - self.last["poll"] > PEER_POLL:
            team = self.gather(use_mb)
            self.deliver(self.analyse(team))
            self.last["poll"] = t
        if use_mb:
            self.mailbox_messages()
            self.last["mailbox"] = t
        if t - self.last["fetch"] > FETCH_EVERY and git_ok("remote", "get-url", "origin"):
            self.last["fetch"] = t
            threading.Thread(target=lambda: git("fetch", "-q", "origin", check=False, timeout=60), daemon=True).start()


def receive_messages(msgs: list[dict]) -> None:
    for m in msgs:
        to = " (to team)" if m.get("to") == "team" else ""
        push_feed(f"✉ {m['from']}{to}: {m['text']}")
        push_inbox([{"key": f"msg|{m['from']}|{m['at']}", "kind": "msg", "text": f"✉ {m['from']}{to}: {m['text']}"}])


def send_message(c: dict, to: str, text: str) -> str:
    msg = {"from": c["name"], "to": to, "text": text, "kind": "note"}
    p = Peers(c, up_port())
    if to != "team" and to not in p.online():
        p.discover()
    if p.send(msg) and to != "team":
        return "delivered directly"
    if c.get("mailbox"):
        Mailbox(c).send(msg)
        return "sent via the mailbox"
    if to == "team" and p.online():
        return "delivered to everyone online"
    return "nobody online to receive it (and no mailbox configured)"


# ---------------------------------------------------------------------- UI


def up_port() -> int | None:
    u = load("up.json", {})
    try:
        os.kill(int(u.get("pid", 0)), 0)
        return int(u["port"])
    except (OSError, ValueError, KeyError):
        return None


def working_on(meta: dict, n: int = 3) -> str:
    syms = sorted((meta.get("symbols") or {}).items(), key=lambda kv: -kv[1])
    if syms:
        parts = [short(s) for s, _ in syms[:n]]
        more = len(syms) - n
        return ", ".join(parts) + (f" +{more}" if more > 0 else "")
    files = list((meta.get("files") or {}))
    return ", ".join(os.path.basename(f) for f in files[:n]) + (f" +{len(files) - n}" if len(files) > n else "") or "-"


def render(c: dict, live: bool = False) -> str:
    w = shutil.get_terminal_size((100, 30)).columns
    team = load("team_cache.json", {})
    me = my_meta()
    seen = load("seen.json", {})
    ts = tailscale_status()
    online = sum(1 for t in team.values() if t.get("online"))
    L = []
    title = f" {bold('concord team')} · {c['project']} · you are {bold(c['name'])}"
    if c.get("urgent"):
        title += " · " + red("urgent")
    L.append(title + dim(f"   {time.strftime('%H:%M:%S')}"))
    net = [f"tailnet {green('●') if ts else dim('○')} {'on' if ts else 'off'}",
           f"{online} teammate{'s' if online != 1 else ''} online"]
    if c.get("mailbox"):
        net.append(("relay " if "vercel.app" not in c["mailbox"]["url"] else "mailbox ") + cyan("●"))
    if c.get("discord"):
        net.append("discord " + cyan("●"))
    L.append(" " + dim(" · ").join(net))
    L.append("")
    L.append(bold(" PEOPLE"))
    rows = [(c["name"], me, True, True)] + [(u, t["meta"], t.get("online"), False) for u, t in sorted(team.items())]
    for name, m, on, mine in rows:
        dot = green("●") if on else dim("○")
        who = name + (" (you)" if mine else "")
        tool = (m.get("tool") or "")[:12]
        line = f" {dot} {who:<14} {tool:<12} {(m.get('branch') or '')[:18]:<18} {working_on(m):<34}"
        state = f"changed {ago(m.get('at'))}" if on else f"offline · last seen {ago(team.get(name, {}).get('seen') or m.get('at'))}"
        L.append(line[: max(40, w - 24)] + " " + dim(state) + (" " + red("urgent") if m.get("urgent") and not mine else ""))
    L.append("")
    if seen:
        mine_first = sorted(seen.values(), key=lambda f: (not f.get("you_fix"), f["key"]))
        L.append(bold(f" CLASHES ({len(seen)})"))
        for f in mine_first:
            icon = red(f["icon"]) if f.get("you_fix") else yellow(f["icon"]) if f["icon"] in "⚠✖" else cyan(f["icon"])
            L.append(f" {icon} {f['head']}")
            L.append("     " + dim(f["detail"][: w - 6]))
            L.append("     " + (yellow(f["verdict"]) if f.get("you_fix") else dim(f["verdict"])))
    else:
        L.append(" " + green("✓ no clashes") + dim(" with anyone right now"))
    L.append("")
    L.append(bold(" FEED"))
    feed = load("feed.json", [])[-8:]
    if not feed:
        L.append(dim("   nothing yet"))
    for e in reversed(feed):
        L.append(f" {dim(time.strftime('%H:%M:%S', time.localtime(e['t'] / 1000)))} {e['text'][: w - 12]}")
    if live:
        L.append("")
        L.append(dim(" concord-team say <name> <message> · concord-team urgent on · ctrl-c to stop"))
    return "\n".join(L)


# ------------------------------------------------------------------ relay


NAME_RE = re.compile(r"^[a-z0-9][a-z0-9_-]{0,39}$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")


class Relay:
    """The always-online team member: stores everyone's latest work and messages
    (the same API as the Vercel mailbox, on disk) and runs the Discord bot."""

    def __init__(self, data: str, key: str):
        self.data = data
        self.key = key
        self.lock = threading.RLock()
        self.discord = Discord(self)

    def pdir(self, project: str, *sub: str) -> str:
        p = os.path.join(self.data, "projects", project, *sub)
        os.makedirs(p, exist_ok=True)
        return p

    def projects(self) -> list[str]:
        root = os.path.join(self.data, "projects")
        return sorted(os.listdir(root)) if os.path.isdir(root) else []

    def jload(self, path: str, default):
        try:
            with open(path) as f:
                return json.load(f)
        except (OSError, ValueError):
            return default

    def jsave(self, path: str, data) -> None:
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(data, f)
        os.replace(tmp, path)

    # -- the mailbox API
    def put_snapshot(self, project: str, user: str, meta: dict, bundle: bytes) -> None:
        with self.lock:
            bpath = os.path.join(self.pdir(project, "bundles"), f"{user}.bundle")
            with open(bpath + ".tmp", "wb") as f:
                f.write(bundle)
            os.replace(bpath + ".tmp", bpath)
            m = {k: meta.get(k) for k in ("sha", "base", "branch", "files", "symbols", "tool", "urgent", "at", "version")}
            m["user"], m["received"] = user, ms()
            self.jsave(os.path.join(self.pdir(project, "meta"), f"{user}.json"), m)

    def seen(self, project: str, user: str) -> None:
        with self.lock:
            path = os.path.join(self.pdir(project), "seen.json")
            s = self.jload(path, {})
            s[user] = ms()
            self.jsave(path, s)

    def state(self, project: str) -> dict:
        out = {}
        seen = self.jload(os.path.join(self.pdir(project), "seen.json"), {})
        mdir = self.pdir(project, "meta")
        for fn in os.listdir(mdir):
            if fn.endswith(".json"):
                m = self.jload(os.path.join(mdir, fn), None)
                if m:
                    m["seen"] = seen.get(m["user"], m.get("received"))
                    out[m["user"]] = m
        return out

    def bundle(self, project: str, user: str, sha: str) -> bytes | None:
        m = self.jload(os.path.join(self.pdir(project, "meta"), f"{user}.json"), {})
        if m.get("sha") != sha:
            return None
        with contextlib.suppress(OSError), open(os.path.join(self.pdir(project, "bundles"), f"{user}.bundle"), "rb") as f:
            return f.read()
        return None

    def put_message(self, project: str, m: dict) -> dict:
        msg = {"from": str(m.get("from", ""))[:60], "to": str(m.get("to", ""))[:40], "text": str(m.get("text", ""))[:4000],
               "kind": str(m.get("kind", "note"))[:20], "at": ms()}
        for k in ("key", "fixer"):
            if m.get(k):
                msg[k] = str(m[k])[:300]
        if isinstance(m.get("parties"), list):
            msg["parties"] = [str(x)[:40] for x in m["parties"][:4]]
        with self.lock:
            path = os.path.join(self.pdir(project), "messages.json")
            msgs = self.jload(path, [])
            msgs.append(msg)
            self.jsave(path, msgs[-1000:])
            if msg["kind"] in ("finding", "resolved") and msg.get("key"):
                fpath = os.path.join(self.pdir(project), "findings.json")
                fs = self.jload(fpath, {})
                fid = hashlib.sha1(msg["key"].encode()).hexdigest()[:12]
                if msg["kind"] == "finding":
                    fs[fid] = {"key": msg["key"], "text": msg["text"], "fixer": msg.get("fixer", ""),
                               "parties": msg.get("parties", []), "at": msg["at"]}
                else:
                    fs.pop(fid, None)
                self.jsave(fpath, fs)
                msg["fid"] = fid
        threading.Thread(target=self.discord.on_message, args=(project, msg), daemon=True).start()
        return msg

    def messages(self, project: str, user: str, since: int) -> list[dict]:
        msgs = self.jload(os.path.join(self.pdir(project), "messages.json"), [])
        return [m for m in msgs if m["at"] > since and m["to"] in (user, "team")]

    def findings(self, project: str) -> dict:
        return self.jload(os.path.join(self.pdir(project), "findings.json"), {})


class Discord:
    """Discord bot over HTTP interactions: slash commands, alert buttons, channel posts."""

    API = "https://discord.com/api/v10"

    def __init__(self, relay: Relay):
        self.relay = relay
        self.app = os.environ.get("DISCORD_APP_ID", "")
        self.public_key = os.environ.get("DISCORD_PUBLIC_KEY", "")
        self.token = os.environ.get("DISCORD_BOT_TOKEN", "")
        self.guild = os.environ.get("DISCORD_GUILD_ID", "")
        self.enabled = bool(self.app and self.public_key and self.token)

    def cfg_path(self) -> str:
        return os.path.join(self.relay.data, "discord.json")

    def cfg(self) -> dict:
        return self.relay.jload(self.cfg_path(), {"channels": {}, "links": {}})

    def save_cfg(self, c: dict) -> None:
        with self.relay.lock:
            self.relay.jsave(self.cfg_path(), c)

    def verify(self, sig: str, ts: str, body: bytes) -> bool:
        try:
            from nacl.signing import VerifyKey
            VerifyKey(bytes.fromhex(self.public_key)).verify(ts.encode() + body, bytes.fromhex(sig))
            return True
        except Exception:
            return False

    def api(self, method: str, path: str, body=None):
        req = urllib.request.Request(
            self.API + path, method=method, data=json.dumps(body).encode() if body is not None else None,
            headers={"authorization": f"Bot {self.token}", "content-type": "application/json",
                     "user-agent": f"DiscordBot (https://github.com/lucas-rus/concord, {VERSION})"})
        with urllib.request.urlopen(req, timeout=10) as r:
            data = r.read()
        return json.loads(data) if data else None

    def register(self) -> str:
        if not self.enabled:
            return "discord: off (set DISCORD_APP_ID, DISCORD_PUBLIC_KEY, DISCORD_BOT_TOKEN)"
        s = lambda n, d, req=False: {"type": 3, "name": n, "description": d, "required": req}
        # integration_types 0/1: installable to a server or to a user's own account;
        # contexts 0/1/2: usable in servers, in a DM with the bot, and in other DMs.
        cmd = [{"name": "team", "description": "Concord team: who is working on what, and clashes",
                "integration_types": [0, 1], "contexts": [0, 1, 2], "options": [
            {"type": 1, "name": "status", "description": "Who is working on what, and open clashes"},
            {"type": 1, "name": "clashes", "description": "Open clashes and who fixes them"},
            {"type": 1, "name": "say", "description": "Message a teammate (their AI sees it too)",
             "options": [s("to", "teammate name, or team", True), s("message", "what to say", True)]},
            {"type": 1, "name": "iam", "description": "Link your Discord account to your team name",
             "options": [s("name", "your concord-team name", True)]},
            {"type": 1, "name": "here", "description": "Post this team's clashes in this channel",
             "options": [s("project", "project (if the relay has several)")]},
        ]}]
        path = f"/applications/{self.app}/guilds/{self.guild}/commands" if self.guild else f"/applications/{self.app}/commands"
        try:
            self.api("PUT", path, cmd)
            return f"discord: commands registered ({'guild ' + self.guild if self.guild else 'global, can take up to an hour'})"
        except Exception as e:
            return f"discord: could not register commands: {e}"

    def project_for(self, channel: str | None, wanted: str | None = None) -> str | None:
        projects = self.relay.projects()
        if wanted:
            return next((p for p in projects if p == wanted or p.startswith(wanted + "-")), None)
        bound = {v: k for k, v in self.cfg()["channels"].items()}
        if channel in bound:
            return bound[channel]
        if len(projects) == 1:
            return projects[0]
        recent = sorted(projects, key=lambda p: os.path.getmtime(self.relay.pdir(p)), reverse=True)
        return recent[0] if recent else None

    def status_text(self, project: str) -> str:
        st = self.relay.state(project)
        team = project.rsplit("-", 1)[0]
        L = [f"**concord team · {team}** · {len(st)} {'person' if len(st) == 1 else 'people'}"]
        for u, m in sorted(st.items()):
            on = ms() - (m.get("seen") or 0) < 90_000
            urgent = " · 🔴 urgent" if m.get("urgent") else ""
            L.append(f"{'🟢' if on else '⚪'} **{u}** ({m.get('tool') or '?'}) · `{m.get('branch')}` · "
                     f"{working_on(m)} · changed {ago(m.get('at'))}{urgent}")
        L.append(self.clashes_text(project))
        return "\n".join(L)[:1990]

    def clashes_text(self, project: str) -> str:
        fs = self.relay.findings(project)
        if not fs:
            return "✅ no clashes"
        return "\n".join([f"**Clashes ({len(fs)})**"] + [f"• {f['text']}" for f in fs.values()])[:1500]

    def buttons(self, fid: str) -> list:
        return [{"type": 1, "components": [
            {"type": 2, "style": 1, "label": "I'm on it", "custom_id": f"onit:{fid}"},
            {"type": 2, "style": 2, "label": "Ask them to fix it", "custom_id": f"ask:{fid}"}]}]

    def on_message(self, project: str, msg: dict) -> None:
        if not self.enabled:
            return
        ch = self.cfg()["channels"].get(project)
        if not ch:
            return
        try:
            if msg["kind"] == "finding":
                self.api("POST", f"/channels/{ch}/messages", {"content": msg["text"][:1900],
                         "components": self.buttons(msg.get("fid", "")), "allowed_mentions": {"parse": []}})
            elif msg["kind"] == "resolved":
                self.api("POST", f"/channels/{ch}/messages", {"content": msg["text"][:1900], "allowed_mentions": {"parse": []}})
            elif msg["to"] == "team" and msg["kind"] == "note" and not msg["from"].endswith("(discord)"):
                self.api("POST", f"/channels/{ch}/messages", {"content": f"✉ **{msg['from']}**: {msg['text']}"[:1900],
                         "allowed_mentions": {"parse": []}})
        except Exception as e:
            print("discord post failed:", e, flush=True)

    def interaction(self, d: dict) -> dict:
        if d.get("type") == 1:
            return {"type": 1}
        user = (d.get("member") or {}).get("user") or d.get("user") or {}
        uid = user.get("id", "")
        display = user.get("global_name") or user.get("username") or "someone"
        links = self.cfg()["links"]
        me = links.get(uid)
        reply = lambda text, private=False: {"type": 4, "data": {"content": text[:1990], "flags": 64 if private else 0,
                                                                  "allowed_mentions": {"parse": []}}}
        if d.get("type") == 3:  # a button
            action, _, fid = (d.get("data") or {}).get("custom_id", "").partition(":")
            project = self.project_for(d.get("channel_id"))
            f = self.relay.findings(project).get(fid) if project else None
            if not f:
                return reply("That clash is already resolved. ✅", True)
            who = me or f"{display} (discord)"
            if action == "onit":
                for p in f.get("parties", []):
                    if p != me and p != "main":
                        self.relay.put_message(project, {"from": who, "to": p, "kind": "note",
                                                         "text": f"I'm on it: {f['text']}"})
                return reply(f"✋ **{who}** is on it: {f['text']}")
            if action == "ask":
                fixer = f.get("fixer") or ""
                if fixer and fixer != "main":
                    self.relay.put_message(project, {"from": who, "to": fixer, "kind": "note",
                                                     "text": f"Please fix this clash: {f['text']}"})
                return reply(f"📨 asked **{fixer or 'the fixer'}** to fix it: {f['text']}")
            return reply("Unknown button.", True)
        data = d.get("data") or {}
        sub = (data.get("options") or [{}])[0]
        opts = {o["name"]: o.get("value") for o in sub.get("options") or []}
        name = sub.get("name")
        if name == "iam":
            n = slug(str(opts.get("name", "")), 20)
            c = self.cfg()
            c["links"][uid] = n
            self.save_cfg(c)
            return reply(f"Linked: you are **{n}** in concord-team.", True)
        project = self.project_for(d.get("channel_id"), opts.get("project") if name == "here" else None)
        if not project:
            return reply("No team has synced with this relay yet: run `concord-team relay <url> <key>` in a clone.", True)
        if name == "here":
            c = self.cfg()
            c["channels"][project] = d.get("channel_id")
            self.save_cfg(c)
            return reply(f"📌 Clashes of **{project.rsplit('-', 1)[0]}** will be posted here.")
        if name == "status":
            return reply(self.status_text(project))
        if name == "clashes":
            return reply(self.clashes_text(project))
        if name == "say":
            to = str(opts.get("to", "")).strip()
            to = "team" if to == "team" else slug(to, 40)
            who = me or f"{display} (discord)"
            self.relay.put_message(project, {"from": who, "to": to, "kind": "note", "text": str(opts.get("message", ""))})
            return reply(f"✉ **{who}** → **{to}**: {opts.get('message', '')}")
        return reply("Unknown command.", True)


class RelayHandler(http.server.BaseHTTPRequestHandler):
    server_version = f"concord-team-relay/{VERSION}"
    relay: Relay = None  # type: ignore[assignment]

    def log_message(self, *a):
        pass

    def _send(self, code: int, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _body(self, limit: int) -> bytes | None:
        n = int(self.headers.get("content-length") or 0)
        if n > limit:
            self._send(413, {"error": "too large"})
            return None
        return self.rfile.read(n)

    def _api(self):
        """(path, query, project) for an authorised API call, or None after answering."""
        u = urllib.parse.urlparse(self.path)
        q = {k: v[0] for k, v in urllib.parse.parse_qs(u.query).items()}
        got = (self.headers.get("authorization") or "").removeprefix("Bearer ").strip()
        if not hmac.compare_digest(got.encode(), self.relay.key.encode()):
            self._send(401, {"error": "unauthorized"})
            return None
        project = q.get("project", "")
        if not NAME_RE.match(project):
            self._send(400, {"error": "bad project"})
            return None
        return u.path, q, project

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        if u.path in ("/", "/healthz"):
            return self._send(200, f"concord-team relay {VERSION}\n".encode(), "text/plain")
        r = self._api()
        if not r:
            return
        path, q, project = r
        if path == "/api/state":
            return self._send(200, {"users": self.relay.state(project), "now": ms()})
        if path == "/api/snapshot":
            user, sha = q.get("user", ""), q.get("sha", "")
            b = self.relay.bundle(project, user, sha) if NAME_RE.match(user) and SHA_RE.match(sha) else None
            return self._send(200, b, "application/octet-stream") if b else self._send(404, {"error": "no such snapshot"})
        if path == "/api/message":
            user = q.get("user", "")
            if not NAME_RE.match(user):
                return self._send(400, {"error": "bad user"})
            self.relay.seen(project, user)
            return self._send(200, {"messages": self.relay.messages(project, user, int(q.get("since") or 0)), "now": ms()})
        self._send(404, {"error": "not found"})

    def do_POST(self):
        u = urllib.parse.urlparse(self.path)
        if u.path == "/discord/interactions":
            body = self._body(1 << 20)
            if body is None:
                return
            dc = self.relay.discord
            if not dc.enabled or not dc.verify(self.headers.get("x-signature-ed25519", ""),
                                               self.headers.get("x-signature-timestamp", ""), body):
                return self._send(401, {"error": "bad signature"})
            try:
                return self._send(200, dc.interaction(json.loads(body)))
            except Exception as e:
                print("discord interaction failed:", e, flush=True)
                return self._send(200, {"type": 4, "data": {"content": "Something went wrong.", "flags": 64}})
        r = self._api()
        if not r:
            return
        path, q, project = r
        if path == "/api/snapshot":
            user = q.get("user", "")
            try:
                meta = json.loads(base64.b64decode(self.headers.get("x-concord-meta", "")))
            except ValueError:
                return self._send(400, {"error": "bad meta"})
            if not NAME_RE.match(user) or not SHA_RE.match(str(meta.get("sha", ""))):
                return self._send(400, {"error": "bad user or sha"})
            body = self._body(50 << 20)
            if not body:
                return
            self.relay.put_snapshot(project, user, meta, body)
            self.relay.seen(project, user)
            return self._send(200, {"ok": True})
        if path == "/api/message":
            body = self._body(65536)
            if body is None:
                return
            try:
                m = json.loads(body)
            except ValueError:
                return self._send(400, {"error": "bad json"})
            if not m.get("from") or not m.get("to") or not m.get("text"):
                return self._send(400, {"error": "from, to and text are required"})
            msg = self.relay.put_message(project, m)
            return self._send(200, {"ok": True, "at": msg["at"]})
        self._send(404, {"error": "not found"})


def _relay_delete(self):
    """DELETE /api/project?project=: forget a team's data on the relay (needs the relay key)."""
    r = self._api()
    if not r:
        return
    path, _, project = r
    if path != "/api/project":
        return self._send(404, {"error": "not found"})
    with self.relay.lock:
        shutil.rmtree(os.path.join(self.relay.data, "projects", project), ignore_errors=True)
        c = self.relay.discord.cfg()
        c["channels"].pop(project, None)
        self.relay.discord.save_cfg(c)
    return self._send(200, {"ok": True, "deleted": project})


RelayHandler.do_DELETE = _relay_delete


def cmd_serve(a):
    key = os.environ.get("RELAY_KEY", "")
    if len(key) < 16:
        sys.exit("set RELAY_KEY (16+ random characters): the key teammates use to reach this relay")
    os.makedirs(a.data, exist_ok=True)
    relay = Relay(a.data, key)
    RelayHandler.relay = relay
    port = int(os.environ.get("PORT") or a.port)
    srv = http.server.ThreadingHTTPServer(("0.0.0.0", port), RelayHandler)
    srv.daemon_threads = True
    print(f"concord-team relay {VERSION} on :{port}, data in {a.data}", flush=True)
    print(relay.discord.register(), flush=True)
    srv.serve_forever()


DEFAULT_RELAY = os.path.expanduser("~/.config/concord-team/relay.json")


def cmd_relay(a):
    relay = {"url": a.url.rstrip("/"), "key": a.key, "poll": 8}
    if a.default:
        os.makedirs(os.path.dirname(DEFAULT_RELAY), exist_ok=True)
        fd = os.open(DEFAULT_RELAY, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(relay, f)
        print(f"{green('✓')} saved as your default relay: teams you `init` from now on use it")
        if not config(False):
            return
    c = config()
    c["mailbox"] = relay
    save("team.json", c)
    try:
        Mailbox(c).states()
        print(f"{green('✓')} relay {a.url} reachable; this clone now syncs through it too")
    except Exception as e:
        print(f"{yellow('!')} saved, but the relay did not answer: {e}")
    print("New invites (`concord-team invite`) include it. Teammates who joined earlier run the same command.")


# ---------------------------------------------------------------- commands


def detect_main() -> str:
    r = git("symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD", check=False)
    if r:
        return r
    for cand in ("origin/main", "origin/master", "main", "master"):
        if git_ok("rev-parse", "--verify", "-q", cand):
            return cand
    return "HEAD"


def detect_name() -> str:
    n = git("config", "user.name", check=False) or os.environ.get("USER", "me")
    return slug(n.split()[0] if n.split() else n, 20)


def exclude_dir() -> None:
    excl = git("rev-parse", "--git-path", "info/exclude")
    excl = excl if os.path.isabs(excl) else os.path.join(ROOT, excl)
    os.makedirs(os.path.dirname(excl), exist_ok=True)
    lines = open(excl).read().splitlines() if os.path.exists(excl) else []
    if ".concord/" not in lines:
        with open(excl, "a") as f:
            f.write("\n.concord/\n")


def require_repo() -> None:
    if not ROOT:
        sys.exit("run this inside your git clone of the project")
    if not git_ok("rev-parse", "--verify", "-q", "HEAD"):
        sys.exit("this repository has no commits yet")


def cmd_init(a):
    require_repo()
    if config(False) and not a.force:
        sys.exit("already in a team here (`concord-team invite` prints the invite; --force to start over)")
    c = {"project": slug(a.project or os.path.basename(ROOT)), "token": secrets.token_urlsafe(24),
         "name": slug(a.name or detect_name(), 20), "main": a.main or detect_main(), "tool": a.tool or "",
         "version": VERSION}
    if a.mailbox:
        if not a.mailbox_key:
            sys.exit("--mailbox needs --mailbox-key")
        c["mailbox"] = {"url": a.mailbox, "key": a.mailbox_key}
    elif os.path.exists(DEFAULT_RELAY):
        with open(DEFAULT_RELAY) as f:
            c["mailbox"] = json.load(f)
    if a.discord:
        c["discord"] = a.discord
    save("team.json", c)
    exclude_dir()
    inv = make_invite(c)
    print(f"\n{green('✓')} team {bold(c['project'])} created · you are {bold(c['name'])} · base {c['main']}\n")
    print("Send your teammate this invite (privately; it is the team's key):\n")
    print("  " + bold(inv) + "\n")
    print("They run, in their clone of this repo:\n")
    print("  brew install lucas-rus/tap/concord-team")
    print(f"  concord-team join {inv[:16]}…   (the whole invite)\n")
    print("Then both of you: `concord-team setup claude` (if you use Claude Code) and `concord-team up`.")


def cmd_join(a):
    require_repo()
    d = read_invite(a.invite)
    name = slug(a.name or detect_name(), 20)
    if name == "main":
        sys.exit("`main` is reserved; pick another --name")
    c = {"project": d["p"], "token": d["t"], "name": name, "main": d.get("main") or detect_main(),
         "tool": a.tool or "", "version": VERSION}
    if d.get("m"):
        c["mailbox"] = d["m"]
    if d.get("d"):
        c["discord"] = d["d"]
    if not git_ok("rev-parse", "--verify", "-q", c["main"]):
        print(yellow(f"! {c['main']} does not exist here yet: run `git fetch origin` (or use --main)"))
    save("team.json", c)
    exclude_dir()
    print(f"{green('✓')} joined {bold(c['project'])} as {bold(name)}")
    print("Next: `concord-team setup claude` (if you use Claude Code), then `concord-team up`.")


def cmd_invite(a):
    print(make_invite(config()))


def cmd_up(a):
    c = config()
    require_repo()
    concord_bin(c)
    for addr in a.peer or []:
        c.setdefault("peers_static", []).append(addr)
    ts = tailscale_status()
    port = start_server(c, tailscale_ip(ts))
    save("up.json", {"pid": os.getpid(), "port": port, "started": ms()})
    sync = Sync(c, port)
    live = sys.stdout.isatty() and not a.plain
    push_feed(f"you are up on port {port}" + (f" · tailnet {tailscale_ip(ts)}" if ts else " · tailnet off"))

    def bye(*_):
        if live:
            sys.stdout.write("\x1b[?25h\n")
        with contextlib.suppress(OSError):
            os.unlink(os.path.join(tdir(), "up.json"))
        os._exit(0)

    signal.signal(signal.SIGINT, bye)
    signal.signal(signal.SIGTERM, bye)
    if live:
        sys.stdout.write("\x1b[?25l\x1b[2J")
    shown = len(load("feed.json", []))
    woke = True
    while True:
        c = config()  # pick up `urgent` and other changes
        sync.c = sync.peers.c = Handler.cfg = c
        try:
            sync.round(force_pull=woke)
        except Exception as e:
            push_feed(f"! {str(e)[:120]}")
        if live:
            out = render(c, live=True).replace("\n", "\x1b[K\n")
            sys.stdout.write("\x1b[H" + out + "\x1b[K\x1b[J")
            sys.stdout.flush()
        else:
            feed = load("feed.json", [])
            for e in feed[shown:] if len(feed) >= shown else feed:
                print(time.strftime("%H:%M:%S", time.localtime(e["t"] / 1000)), e["text"], flush=True)
            shown = len(feed)
        woke = WAKE.wait(2)
        WAKE.clear()


def one_shot(c: dict) -> None:
    """A single sync round, unless `up` is running (then just wake it)."""
    port = up_port()
    if port:
        with contextlib.suppress(Exception):
            call("POST", f"http://127.0.0.1:{port}/api/poke?project={c['project']}", c["token"], body=b"{}", timeout=2)
        return
    with locked(".sync.lock", block=False) as got:
        if got:
            s = Sync(c)
            s.round(force_pull=True)


def cmd_status(a):
    c = config()
    if not up_port():
        one_shot(c)
    else:
        one_shot(c)
        time.sleep(1.5)
    print(render(c))


def cmd_sync(a):
    one_shot(config())


def cmd_say(a):
    c = config()
    text = " ".join(a.text)
    how = send_message(c, slug(a.to, 40) if a.to != "team" else "team", text)
    push_feed(f"→ you to {a.to}: {text}")
    print(f"{green('✓')} {how}")


def cmd_urgent(a):
    c = config()
    c["urgent"] = a.state == "on"
    save("team.json", c)
    one_shot(c)
    print(f"{green('✓')} your work is {'urgent: you win clashes until you turn it off' if c['urgent'] else 'normal'}")
    with contextlib.suppress(Exception):
        send_message(c, "team", f"{c['name']} marked their work {'URGENT' if c['urgent'] else 'normal again'}")


def take_inbox() -> list[dict]:
    with locked():
        inbox = load("inbox.json", [])
        save("inbox.json", [])
    # a clash that appeared and resolved before the AI saw it is not news
    resolved = {i["key"] for i in inbox if i["kind"] == "resolved"}
    fresh = {i["key"] for i in inbox if i["kind"] == "new"}
    return [i for i in inbox if not (i["key"] in resolved and i["key"] in fresh)]


def brief(c: dict) -> str:
    team = load("team_cache.json", {})
    seen = load("seen.json", {})
    L = ["[concord team] You are working in a shared repository. Other people are working on it at the same time,",
         "each with their own AI. Concord watches for clashes between your work and theirs."]
    if team:
        L.append("Teammates right now:")
        for u, t in sorted(team.items()):
            m = t["meta"]
            state = "online" if t.get("online") else "offline"
            L.append(f"- {u} ({m.get('tool') or 'unknown tool'}, {state}, branch {m.get('branch')}): "
                     f"working on {working_on(m, 6)}; last change {ago(m.get('at'))}")
    if seen:
        L.append("Open clashes:")
        for f in seen.values():
            L.append(f"- {f['head']} {f['verdict']}. What to do: {f['ai']}")
    L.append('To message a teammate (or their AI): concord-team say <name> "<message>". Team overview: concord-team status.')
    return "\n".join(L)


def cmd_hook(a):
    """Claude Code hook for SessionStart, UserPromptSubmit and PostToolUse. Never fails the agent."""
    global ROOT
    try:
        data = json.loads(sys.stdin.read() or "{}")
    except ValueError:
        data = {}
    try:
        ROOT = find_root(data.get("cwd")) or ROOT
        c = config(False) if ROOT else None
        if not c:
            return
        event = data.get("hook_event_name") or "PostToolUse"
        if up_port():
            one_shot(c)
        else:
            subprocess.Popen([sys.executable, os.path.abspath(__file__), "sync"], cwd=ROOT,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        parts = []
        if event == "SessionStart":
            parts.append(brief(c))
            take_inbox()
        else:
            items = take_inbox()
            if items:
                parts.append("[concord team] Updates from your teammates (other people on this repo, with their own AI):")
                parts += [i["text"] for i in items]
                parts.append('Reply to a teammate with: concord-team say <name> "<message>"')
        if parts:
            text = "\n".join(parts).replace("concord-team say", f"{self_cmd()} say").replace(
                "concord-team status", f"{self_cmd()} status")
            print(json.dumps({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}))
    except Exception:
        pass


def cmd_statusline(a):
    """One line for Claude Code's status bar (reads local files only)."""
    global ROOT
    try:
        data = json.loads(sys.stdin.read() or "{}")
    except ValueError:
        data = {}
    ROOT = find_root((data.get("workspace") or {}).get("current_dir") or data.get("cwd")) or ROOT
    c = config(False) if ROOT else None
    if not c:
        return
    team = load("team_cache.json", {})
    seen = load("seen.json", {})
    on = paint("32", "●", True)
    off = paint("2", "○", True)
    people = [f"{on if t.get('online') else off} {u} {paint('2', working_on(t['meta'], 2), True)}"
              for u, t in sorted(team.items())]
    s = "👥 " + ("  ".join(people) if people else paint("2", "no teammates seen yet", True))
    if seen:
        mine = sum(1 for f in seen.values() if f.get("you_fix"))
        s += "  │ " + paint("31" if mine else "33", f"⚠ {len(seen)} clash{'es' if len(seen) != 1 else ''}"
                          + (f" ({mine} for you)" if mine else ""), True)
    print(s)


def self_cmd() -> str:
    exe = shutil.which("concord-team")
    if exe and os.path.realpath(exe) == os.path.realpath(__file__):
        return "concord-team"
    return f"{shlex_quote(sys.executable)} {shlex_quote(os.path.abspath(__file__))}"


def shlex_quote(s: str) -> str:
    import shlex
    return shlex.quote(s)


def cmd_setup(a):
    c = config()
    if a.client != "claude":
        sys.exit("only `claude` for now (Cursor and OpenCode are next)")
    path = os.path.join(ROOT, ".claude", "settings.local.json")
    try:
        with open(path) as f:
            s = json.load(f)
    except (OSError, ValueError):
        s = {}
    cmd = f"{self_cmd()} hook"
    hooks = s.setdefault("hooks", {})
    for event, extra in (("SessionStart", {}), ("UserPromptSubmit", {}), ("PostToolUse", {"matcher": "*"})):
        groups = hooks.setdefault(event, [])
        if not any(h.get("command") == cmd for g in groups for h in g.get("hooks", [])):
            groups.append({**extra, "hooks": [{"type": "command", "command": cmd, "timeout": 10}]})
    note = ""
    sl = s.get("statusLine")
    if not sl or "concord-team" in json.dumps(sl) or "concord_team" in json.dumps(sl):
        s["statusLine"] = {"type": "command", "command": f"{self_cmd()} statusline"}
    else:
        note = f"\n{yellow('!')} you already have a status line here, so I left it alone"
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(s, f, indent=2)
    c["tool"] = c.get("tool") or "claude-code"
    save("team.json", c)
    print(f"{green('✓')} Claude Code is connected in this repo ({os.path.relpath(path, ROOT)}):")
    print("  · a team brief when a session starts\n  · teammates' clashes in the chat as they happen")
    print("  · who is working on what, in the status line" + note)
    print("Restart Claude Code in this folder to load it.")


def cmd_doctor(a):
    ok = lambda b: green("✓") if b else red("✖")
    print(f"concord-team {VERSION}")
    gv = subprocess.run(["git", "--version"], capture_output=True, text=True).stdout.strip()
    print(f" {ok(bool(gv))} {gv or 'git missing'}")
    c = config(False)
    try:
        cb = concord_bin(c or {})
        v = subprocess.run([cb, "--version"], capture_output=True, text=True).stdout.strip()
        print(f" {ok(True)} {v} ({cb})")
    except SystemExit:
        print(f" {ok(False)} concord missing: brew install lucas-rus/tap/concord")
    ts = tailscale_status()
    if ts:
        peers = [p for p in (ts.get("Peer") or {}).values() if p.get("Online")]
        print(f" {ok(True)} tailscale up as {tailscale_ip(ts)} · {len(peers)} device(s) online in your tailnet")
    elif tailscale_bin():
        print(f" {yellow('!')} tailscale installed but not connected: open Tailscale and log in")
    else:
        print(f" {yellow('!')} tailscale not installed: brew install --cask tailscale (needed to reach teammates directly)")
    if not c:
        print(f" {yellow('!')} not in a team in this folder")
        return
    print(f" {ok(True)} team {c['project']} as {c['name']} · base {c['main']}"
          + ("" if git_ok("rev-parse", "--verify", "-q", c["main"]) else red(" (missing: git fetch origin)")))
    print(f" {ok(bool(up_port()))} concord-team up {'running on port ' + str(up_port()) if up_port() else 'not running'}")
    found = Peers(c, up_port()).discover()
    print(f" {ok(bool(found))} teammates reachable now: {', '.join(found) or 'none (are they running `concord-team up`?)'}")
    if c.get("mailbox"):
        mb = Mailbox(c)
        try:
            mb.states()
            print(f" {ok(True)} mailbox {mb.url}")
        except Exception as e:
            print(f" {ok(False)} mailbox {mb.url}: {str(e)[:80]}")
    path = os.path.join(ROOT, ".claude", "settings.local.json")
    hooked = os.path.exists(path) and "concord" in open(path).read()
    print(f" {ok(hooked)} Claude Code {'connected' if hooked else 'not connected (concord-team setup claude)'}")


def main():
    global ROOT
    ROOT = find_root()
    p = argparse.ArgumentParser(prog="concord-team", description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--version", action="version", version=f"concord-team {VERSION}")
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("init", help="start a team in this repo and print an invite")
    s.add_argument("--project")
    s.add_argument("--name")
    s.add_argument("--main", help="the shared base branch (default: origin's default branch)")
    s.add_argument("--tool", help="your AI tool, shown to teammates")
    s.add_argument("--mailbox", help="optional mailbox URL for offline delivery")
    s.add_argument("--mailbox-key")
    s.add_argument("--discord", help="Discord webhook URL for the team channel")
    s.add_argument("--force", action="store_true")
    s.set_defaults(f=cmd_init)
    s = sub.add_parser("join", help="join a team with an invite")
    s.add_argument("invite")
    s.add_argument("--name")
    s.add_argument("--tool")
    s.set_defaults(f=cmd_join)
    sub.add_parser("invite", help="print this team's invite again").set_defaults(f=cmd_invite)
    s = sub.add_parser("up", help="live team view (keep it running)")
    s.add_argument("--peer", action="append", help="extra peer address host:port")
    s.add_argument("--plain", action="store_true", help="log lines instead of the live view")
    s.set_defaults(f=cmd_up)
    sub.add_parser("status", help="one-shot overview").set_defaults(f=cmd_status)
    sub.add_parser("sync", help=argparse.SUPPRESS).set_defaults(f=cmd_sync)
    s = sub.add_parser("say", help="message a teammate (or `team`)")
    s.add_argument("to")
    s.add_argument("text", nargs="+")
    s.set_defaults(f=cmd_say)
    s = sub.add_parser("urgent", help="mark your work urgent (you win clashes) or normal")
    s.add_argument("state", choices=["on", "off"])
    s.set_defaults(f=cmd_urgent)
    s = sub.add_parser("setup", help="connect an AI tool (claude)")
    s.add_argument("client", choices=["claude"])
    s.set_defaults(f=cmd_setup)
    sub.add_parser("doctor", help="check the setup").set_defaults(f=cmd_doctor)
    s = sub.add_parser("relay", help="sync through an always-online relay too (url key)")
    s.add_argument("url")
    s.add_argument("key")
    s.add_argument("--default", action="store_true", help="also use it for teams you init later")
    s.set_defaults(f=cmd_relay)
    s = sub.add_parser("serve", help="run the always-online relay and Discord bot (for a server)")
    s.add_argument("--data", default="./concord-relay-data")
    s.add_argument("--port", type=int, default=8080)
    s.set_defaults(f=cmd_serve)
    sub.add_parser("hook", help=argparse.SUPPRESS).set_defaults(f=cmd_hook)
    sub.add_parser("statusline", help=argparse.SUPPRESS).set_defaults(f=cmd_statusline)
    a = p.parse_args()
    a.f(a)


if __name__ == "__main__":
    main()
