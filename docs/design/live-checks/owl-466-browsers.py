#!/usr/bin/env python3
"""OWL-466, live check U3 of docs/design/build-plan.md, "P8 · Local web UI": the browsers.

A throwaway model of the UI's login and session rules (architecture section 5, "Who may drive it"), not the UI:
two listeners on 127.0.0.1, the "UI" on PORT and a foreign page on PORT+1 standing for an agent's listener. The
maintainer opens them in Safari, Chrome and Firefox; the listeners judge each clause from what they receive and
print one PASS or FAIL line per browser and clause. Standard library only.

    python3 docs/design/live-checks/owl-466-browsers.py serve [--port 18466]   # prints a login link, serves
    python3 docs/design/live-checks/owl-466-browsers.py link [--port N]        # a new one-time link (60 s), for
                                                                              # another browser or a tunnel's port
    python3 docs/design/live-checks/owl-466-browsers.py report                 # the summary so far

State (token, session hashes, used codes, results) lives in a 0700 folder outside the repository,
$TMPDIR/owl466-browsers, kept across restarts of `serve`; remove it when done. Nothing printed holds the token or
a session secret; a login link holds a one-time code valid 60 seconds: keep it out of chats and pull requests.
"""

import argparse
import hashlib
import hmac
import json
import os
import re
import secrets
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

STATE = os.environ.get("OWL466_STATE") or os.path.join(tempfile.gettempdir(), "owl466-browsers")
HEADER = "X-Owlshift-Session"
CSP = "default-src 'self'; frame-ancestors 'none'"
LOCK = threading.Lock()


def state_path(name):
    return os.path.join(STATE, name)


def load(name, default):
    try:
        with open(state_path(name)) as f:
            return json.load(f)
    except FileNotFoundError:
        return default


def save(name, value):
    path = state_path(name)
    with open(path + ".new", "w") as f:
        json.dump(value, f)
    os.chmod(path + ".new", 0o600)
    os.replace(path + ".new", path)


def token():
    os.makedirs(STATE, mode=0o700, exist_ok=True)
    os.chmod(STATE, 0o700)
    path = state_path("token")
    if not os.path.exists(path):
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.write(fd, secrets.token_hex(32).encode())
        os.close(fd)
    with open(path) as f:
        return bytes.fromhex(f.read().strip())


def mac(key, text):
    return hmac.new(key, text.encode(), hashlib.sha256).hexdigest()


def login_link(port):
    nonce, at = secrets.token_hex(16), int(time.time())
    code = f"{nonce}.{at}.{mac(token(), f'{nonce}|{at}')}"
    return f"http://127.0.0.1:{port}/#login={code}"


def digest(secret):
    return hashlib.sha256(secret.encode()).hexdigest()


def browser(agent):
    if "Firefox/" in agent:
        return "Firefox"
    if "Edg/" in agent:
        return "Edge"
    if "Chrome/" in agent:
        return "Chrome"
    if "Safari/" in agent:
        return "Safari"
    return "Other"


def record(who, clause, ok, detail=""):
    """Keeps a verdict and prints it once it changes. A FAIL is never overwritten by a PASS."""
    with LOCK:
        results = load("results.json", {})
        clauses = results.setdefault(who, {})
        old = clauses.get(clause)
        if old and old[0] == "FAIL" and ok != "FAIL":
            return
        if old != [ok, detail]:
            clauses[clause] = [ok, detail]
            save("results.json", results)
            print(f"[{who}] {clause}: {ok}{' - ' + detail if detail else ''}", flush=True)


def note(who, key, value=True):
    with LOCK:
        notes = load("notes.json", {})
        notes.setdefault(who, {})[key] = value
        save("notes.json", notes)


def noted(who, key):
    return load("notes.json", {}).get(who, {}).get(key)


UI_PAGE = """<!doctype html><meta charset="utf-8"><title>OWL-466 UI probe</title>
<h1>OWL-466 UI probe</h1><pre id="out"></pre><script src="/ui.js"></script>"""

UI_JS = r"""
const out = document.getElementById('out');
const say = (t) => { out.textContent += t + '\n'; };
if (window.top !== window.self) navigator.sendBeacon('/api/framed');
(async () => {
  let tab = sessionStorage.getItem('owl466-tab');
  if (!tab) { tab = Math.random().toString(16).slice(2); sessionStorage.setItem('owl466-tab', tab); }
  let fresh = 0;
  const m = location.hash.match(/login=([^&]+)/);
  if (m) {
    const r = await fetch('/api/login', {method: 'POST', headers: {'Content-Type': 'application/json'},
                                         body: JSON.stringify({code: m[1]})});
    history.replaceState(null, '', location.pathname);
    if (r.ok) { localStorage.setItem('owlshift-session', (await r.json()).secret); fresh = 1; say('logged in'); }
    else say('login refused: ' + r.status);
  }
  say('address bar now: ' + location.href);
  const secret = localStorage.getItem('owlshift-session');
  if (!secret) { say('no session here: mint a link with `link`'); return; }
  const h = {'X-Owlshift-Session': secret};
  const canary = await (await fetch('/api/canary.json')).json();
  say('own canary read: ' + canary.canary);
  const who = await fetch(`/api/whoami?tab=${tab}&fresh=${fresh}&hash=${location.hash ? 1 : 0}`, {headers: h});
  say('whoami: ' + who.status);
  await fetch('/api/cookie-demo');
  const t0 = performance.now(); const gaps = []; let last = null; let first = null;
  const s = await fetch('/api/stream', {headers: h});
  const reader = s.body.getReader();
  for (;;) {
    const {done, value} = await reader.read();
    if (done) break;
    const now = performance.now();
    if (first === null) first = now - t0;
    if (last !== null && now - last < 200) continue; // one chunk read in pieces
    if (last !== null) gaps.push((now - last) / 1000);
    last = now;
    say('stream chunk at ' + ((now - t0) / 1000).toFixed(2) + ' s: ' + new TextDecoder().decode(value).trim());
  }
  await fetch('/api/report', {method: 'POST', headers: {...h, 'Content-Type': 'application/json'},
    body: JSON.stringify({clause: 'stream', first: first / 1000, gaps})});
  say('done; now open the foreign page on port ' + (Number(location.port) + 1) + ' (step 4)');
})();
"""

CONTROL_PAGE = """<!doctype html><meta charset="utf-8"><title>frame control</title><script src="/control.js"></script>"""
CONTROL_JS = "if (window.top !== window.self) navigator.sendBeacon('/api/framed-control');"

FOREIGN_PAGE = """<!doctype html><meta charset="utf-8"><title>OWL-466 foreign page</title>
<h1>Foreign page (stands for an agent's listener)</h1><pre id="out"></pre>
<iframe id="f1" width="300" height="80"></iframe><iframe id="f2" width="300" height="80"></iframe>
<script src="/foreign.js"></script>"""

FOREIGN_JS = r"""
const out = document.getElementById('out');
const ui = 'http://127.0.0.1:' + (Number(location.port) - 1);
const say = (t) => { out.textContent += t + '\n'; };
const report = (clause, value) => fetch('/report', {method: 'POST', body: JSON.stringify({clause, value})});
(async () => {
  localStorage.setItem('owlshift-session-probe', 'x');
  const control = localStorage.getItem('owlshift-session-probe') === 'x';
  const value = localStorage.getItem('owlshift-session');
  say('own storage works: ' + control + '; UI secret readable here: ' + (value !== null));
  await report('storage', {control, readable: value !== null});
  let corsRead = 'threw';
  try { const r = await fetch(ui + '/api/canary.json?from=foreign'); corsRead = 'read: ' + (await r.text()); }
  catch (e) {}
  let opaque = 'threw';
  try { const r = await fetch(ui + '/api/canary.json?from=foreign', {mode: 'no-cors'}); opaque = r.type; }
  catch (e) {}
  say('cors read of the canary: ' + corsRead + '; no-cors response type: ' + opaque);
  await report('json', {corsRead, opaque});
  try { await fetch(ui + '/api/whoami?from=foreign', {mode: 'no-cors', credentials: 'include'}); } catch (e) {}
  let header = 'threw';
  try { await fetch(ui + '/api/whoami?from=foreign-header', {headers: {'X-Owlshift-Session': 'made-up'}});
        header = 'sent'; } catch (e) {}
  say('request with a session header: ' + header);
  await report('header', {header});
  try { await fetch(ui + '/api/action?from=foreign', {method: 'POST', mode: 'no-cors', body: 'pause'}); }
  catch (e) {}
  document.getElementById('f1').src = ui + '/';
  document.getElementById('f2').src = ui + '/frame-control';
  await new Promise((r) => setTimeout(r, 4000));
  await report('done', {});
  say('done: read the terminal');
})();
"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    port = 0

    def log_message(self, *args):
        pass  # no request line or header is ever printed

    def who(self):
        name = browser(self.headers.get("User-Agent", ""))
        host_port = (self.headers.get("Host") or "").rpartition(":")[2]
        return name if host_port == str(Handler.port) else f"{name} via port {host_port}"

    def send(self, status, body=b"", kind="text/plain", extra=()):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Cache-Control", "no-store")
        for name, value in extra:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""


def valid_session(value):
    return bool(value) and digest(value) in load("sessions.json", [])


class UiHandler(Handler):
    def common(self):
        who = self.who()
        if "login=" in self.path or "login=" in (self.headers.get("Referer") or ""):
            record(who, "fragment stays out of the server", "FAIL", "a request carried login=")
        host = (self.headers.get("Host") or "").rpartition(":")[0] or self.headers.get("Host", "")
        if host not in ("127.0.0.1", "localhost", "[::1]"):
            self.send(421, "host refused")
            return None
        query = parse_qs(urlsplit(self.path).query)
        foreign = query.get("from", [""])[0].startswith("foreign")
        carried = self.headers.get(HEADER)
        if foreign and valid_session(carried):
            record(who, "foreign page cannot have a request carry the secret", "FAIL", "it did")
        if foreign and carried and self.command != "OPTIONS":
            record(who, "foreign page cannot add the header", "FAIL", "a request with the header arrived")
        if foreign:
            note(who, f"foreign {self.command} {urlsplit(self.path).path}")
        return who, query, foreign, carried

    def do_OPTIONS(self):
        seen = self.common()
        if seen:
            self.send(405)  # no CORS header, ever

    def do_GET(self):
        seen = self.common()
        if not seen:
            return
        who, query, foreign, carried = seen
        path = urlsplit(self.path).path
        if path == "/":
            self.send(200, UI_PAGE, "text/html; charset=utf-8", [("Content-Security-Policy", CSP)])
        elif path == "/ui.js":
            self.send(200, UI_JS, "text/javascript")
        elif path == "/frame-control":
            self.send(200, CONTROL_PAGE, "text/html; charset=utf-8", [("Content-Security-Policy", "default-src 'self'")])
        elif path == "/control.js":
            self.send(200, CONTROL_JS, "text/javascript")
        elif path == "/api/canary.json":
            self.send(200, json.dumps({"canary": "owl466"}), "application/json")
        elif path == "/api/cookie-demo":
            # Only to show why the design sets no cookie: the foreign port receives it.
            self.send(204, extra=[("Set-Cookie", "owl466_canary=1; Path=/; SameSite=Strict")])
        elif not valid_session(carried):
            self.send(401, json.dumps({"error": "no session"}), "application/json")
        elif path == "/api/whoami":
            self.whoami(who, query)
            self.send(200, json.dumps({"ok": True}), "application/json")
        elif path == "/api/stream":
            self.stream()
        else:
            self.send(404)

    def whoami(self, who, query):
        start = load("starts.json", 0)
        if query.get("hash", ["0"])[0] == "0" and query.get("fresh", ["0"])[0] == "1":
            note(who, "login start", start)
            note(who, "login tab", query.get("tab", [""])[0])
            record(who, "fragment stays out of the server", "PASS",
                   "logged in from the fragment, dropped from the address bar, never in a request")
        tab = query.get("tab", [""])[0]
        if noted(who, "login tab") and tab != noted(who, "login tab"):
            record(who, "secret survives a new tab", "PASS")
        login_start = noted(who, "login start")
        if login_start is not None and start > login_start:
            record(who, "secret survives a restart of the UI", "PASS")

    def stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.send_header("Transfer-Encoding", "chunked")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        for n in range(5):
            line = json.dumps({"n": n, "t": time.time()}).encode() + b"\n" + b" " * 2048 + b"\n"
            self.wfile.write(f"{len(line):x}\r\n".encode() + line + b"\r\n")
            self.wfile.flush()
            time.sleep(1)
        self.wfile.write(b"0\r\n\r\n")

    def do_POST(self):
        seen = self.common()
        if not seen:
            return
        who, query, foreign, carried = seen
        path = urlsplit(self.path).path
        origin = self.headers.get("Origin")
        own = f"http://{self.headers.get('Host')}"
        data = self.body()
        if origin is not None and origin != own:
            if path == "/api/action":
                record(who, "foreign page cannot make a change", "PASS", "its POST arrived and was refused")
            self.send(403, "foreign origin")
        elif path == "/api/login":
            self.login(data)
        elif path == "/api/framed":
            record(who, "foreign page cannot frame the UI", "FAIL", "the framed page ran")
            self.send(204)
        elif path == "/api/framed-control":
            note(who, "frame control ran")
            self.send(204)
        elif not valid_session(carried):
            if path == "/api/action" and foreign:
                record(who, "foreign page cannot make a change", "PASS", "its POST arrived and was refused")
            self.send(401)
        elif path == "/api/action":
            if foreign:
                record(who, "foreign page cannot make a change", "FAIL", "the action ran")
            self.send(204)
        elif path == "/api/report":
            report = json.loads(data or b"{}")
            if report.get("clause") == "stream":
                first, gaps = report.get("first") or 99, report.get("gaps") or []
                ok = first < 2 and sum(g >= 0.7 for g in gaps) >= 3
                record(who, "streamed fetch carries the header and delivers as it comes",
                       "PASS" if ok else "FAIL", f"first chunk {first:.2f} s, gaps {[round(g, 2) for g in gaps]}")
            self.send(204)
        else:
            self.send(404)

    def login(self, data):
        try:
            nonce, at, code = json.loads(data)["code"].split(".")
            fresh = 0 <= time.time() - int(at) <= 60
            good = hmac.compare_digest(code, mac(token(), f"{nonce}|{at}"))
        except (ValueError, KeyError):
            fresh = good = False
        with LOCK:
            used = load("used.json", [])
            if not (fresh and good) or nonce in used:
                self.send(403, "code refused")
                return
            used.append(nonce)
            save("used.json", used)
            secret = secrets.token_hex(32)
            save("sessions.json", load("sessions.json", []) + [digest(secret)])
        self.send(200, json.dumps({"secret": secret}), "application/json")


class ForeignHandler(Handler):
    def check(self, data=b""):
        who = self.who().split(" via ")[0]
        if "owl466_canary" in (self.headers.get("Cookie") or ""):
            note(who, "cookie reached the foreign port")
        everything = (self.path + str(self.headers)).encode() + data
        sessions = set(load("sessions.json", []))
        # Only hashes are kept: compare the hash of every run of 64 hex digits.
        if any(digest(word.decode()) in sessions for word in re.findall(rb"[0-9a-f]{64}", everything)):
            record(who, "foreign page receives no secret", "FAIL", "a request to it carried one")
        return who

    def do_GET(self):
        self.check()
        path = urlsplit(self.path).path
        if path == "/":
            self.send(200, FOREIGN_PAGE, "text/html; charset=utf-8")
        elif path == "/foreign.js":
            self.send(200, FOREIGN_JS, "text/javascript")
        else:
            self.send(404)

    def do_POST(self):
        data = self.body()
        who = self.check(data)
        report = json.loads(data or b"{}")
        clause, value = report.get("clause"), report.get("value") or {}
        if clause == "storage":
            ok = value.get("control") and not value.get("readable")
            record(who, "foreign page cannot read the secret", "PASS" if ok else "FAIL",
                   f"own storage works: {value.get('control')}, secret readable: {value.get('readable')}")
        elif clause == "json":
            ok = value.get("corsRead") == "threw" and value.get("opaque") == "opaque"
            record(who, "foreign page cannot read a JSON response", "PASS" if ok else "FAIL",
                   f"cors {value.get('corsRead')}, no-cors {value.get('opaque')}; the UI's own page reads it")
        elif clause == "header":
            ok = value.get("header") == "threw" and noted(who, "foreign OPTIONS /api/whoami")
            record(who, "foreign page cannot add the header", "PASS" if ok else "FAIL",
                   f"fetch {value.get('header')}, preflight seen: {bool(noted(who, 'foreign OPTIONS /api/whoami'))}")
        elif clause == "done":
            arrived = noted(who, "foreign GET /api/whoami") or noted(who, "foreign GET /api/canary.json")
            record(who, "foreign page cannot have a request carry the secret",
                   "PASS" if arrived else "FAIL", "its requests reached the UI without a valid secret"
                   if arrived else "none of its requests reached the UI: inconclusive")
            control = noted(who, "frame control ran")
            record(who, "foreign page cannot frame the UI", "PASS" if control else "FAIL",
                   "the framed UI did not run, the unprotected control frame did" if control
                   else "the control frame did not run either: inconclusive")
            record(who, "foreign page receives no secret", "PASS",
                   "no secret in any request it received; a cookie the UI port set reached it: "
                   + ("yes" if noted(who, "cookie reached the foreign port") else "no"))
        self.send(204)


def serve(port):
    token()
    starts = load("starts.json", -1) + 1
    save("starts.json", starts)
    Handler.port = port
    ui = ThreadingHTTPServer(("127.0.0.1", port), UiHandler)
    foreign = ThreadingHTTPServer(("127.0.0.1", port + 1), ForeignHandler)
    threading.Thread(target=foreign.serve_forever, daemon=True).start()
    print(f"start {starts}: UI on http://127.0.0.1:{port}/, foreign page on http://127.0.0.1:{port + 1}/")
    print(f"login link (one browser, 60 s): {login_link(port)}", flush=True)
    try:
        ui.serve_forever()
    except KeyboardInterrupt:
        print("\nstopped")
        summary()


def summary():
    for who, clauses in sorted(load("results.json", {}).items()):
        for clause, (ok, detail) in sorted(clauses.items()):
            print(f"{who:24} {ok:4} {clause}{' - ' + detail if detail else ''}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=["serve", "link", "report"])
    parser.add_argument("--port", type=int, default=18466)
    args = parser.parse_args()
    if args.command == "serve":
        serve(args.port)
    elif args.command == "link":
        token()
        print(login_link(args.port))
    else:
        summary()


if __name__ == "__main__":
    sys.exit(main())
