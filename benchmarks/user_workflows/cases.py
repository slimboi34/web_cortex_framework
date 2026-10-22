"""Real-use scenarios for WebCortex, run against two builds side by side.

Each :class:`Case` is a snippet that starts real apps (``harness.serve``) and
talks to them as a user, a browser, a device or an MCP client would. It leaves
a dict of observations in ``result``; ``expect`` maps some of those keys to the
value a correct framework produces. A case passes when every expected key
matches. Every case runs in a fresh interpreter, so a crash or hang in one
cannot leak into the next.

    python cases.py                       # both builds, a table
    python cases.py --build dev           # one build
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import textwrap
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
BASELINE_VERSION = "2.5.3"


@dataclass
class Case:
    section: str
    label: str
    code: str
    expect: Dict[str, Any]
    why: str = ""
    everyday: bool = False      # ordinary use: both builds should agree


CASES: List[Case] = []


def case(section, label, expect, why="", everyday=False):
    def register(fn_or_code):
        code = fn_or_code if isinstance(fn_or_code, str) else fn_or_code.__doc__
        CASES.append(Case(section, label, textwrap.dedent(code), expect, why, everyday))
        return fn_or_code
    return register


BOOKSTORE = '''
from webcortex import WebCortex, HTTPError

app = WebCortex("bookstore", database="sqlite://./app.db")
app.api_key("SHOP_KEY", id="service", scopes=["read", "write", "webcortex:admin"])
app.anonymous_scopes("read")
app.resource("books", fields={"id": int, "title": str, "author": str, "year": int},
             tools=True, read_scopes=["read"], write_scopes=["write"])

@app.get("/books/{id}/blurb", tool=True, scopes=["read"])
def blurb(id: int) -> str:
    """One-line pitch for a book."""
    return f"Book {id} - highly recommended."

@app.post("/checkout", scopes=["write"])
def checkout(book_id: int, qty: int = 1) -> dict:
    if qty < 1:
        raise HTTPError(422, "qty must be at least 1")
    return {"book_id": book_id, "qty": qty}
'''

KEY = {"x-api-key": "shop-key-0123456789abcdef0123456789"}
ENV = {"SHOP_KEY": KEY["x-api-key"]}

# ---------------------------------------------------------------------------
# Everyday use — both builds should agree
# ---------------------------------------------------------------------------

CASES.append(Case("REST", "bookstore CRUD round trip", textwrap.dedent(f'''
with serve(BOOKSTORE, env=ENV) as s:
    created = [s.post("/books", {{"title": t, "author": a, "year": y}}, headers=KEY)
               for t, a, y in [("Dune", "Herbert", 1965), ("Emma", "Austen", 1815), ("Ubik", "Dick", 1969)]]
    st, _, listing = s.get("/books")
    one = s.get("/books/2")
    anon_write = s.post("/books", {{"title": "x", "author": "y", "year": 1}})[0]
    deleted = s.request("DELETE", "/books/3", headers=KEY)[0]
    after = s.get("/books/3")[0]
    result = {{
        "created": [c[0] for c in created],
        "listed": sorted(b["title"] for b in (listing if isinstance(listing, list) else listing.get("items", []))),
        "get_one": one[2].get("title") if isinstance(one[2], dict) else one[2],
        "anon_write": anon_write, "deleted": deleted in (200, 204), "after_delete": after,
    }}
'''), {"created": [200, 200, 200], "listed": ["Dune", "Emma", "Ubik"], "get_one": "Emma",
       "anon_write": 401, "deleted": True, "after_delete": 404}, everyday=True))

CASES.append(Case("REST", "Python handlers: typed params, errors, defaults", textwrap.dedent('''
with serve(BOOKSTORE, env=ENV) as s:
    ok = s.post("/checkout", {"book_id": 4, "qty": 2}, headers=KEY)
    default = s.post("/checkout", {"book_id": 4}, headers=KEY)
    bad = s.post("/checkout", {"book_id": 4, "qty": 0}, headers=KEY)
    blurb = s.get("/books/7/blurb")
    result = {"ok": [ok[0], ok[2]], "default_qty": default[2]["qty"], "bad": bad[0],
              "bad_has_message": "qty" in json.dumps(bad[2]), "blurb": blurb[2]}
'''), {"ok": [200, {"book_id": 4, "qty": 2}], "default_qty": 1, "bad": 422, "bad_has_message": True,
       "blurb": "Book 7 - highly recommended."}, everyday=True))

CASES.append(Case("REST", "OpenAPI lists every route", textwrap.dedent('''
with serve(BOOKSTORE, env=ENV) as s:
    st, _, spec = s.get("/_webcortex/openapi.json", headers=KEY)
    paths = sorted(spec.get("paths", {})) if isinstance(spec, dict) else []
    result = {"status": st, "has_books": "/books" in paths, "has_blurb": "/books/{id}/blurb" in paths,
              "has_checkout": "/checkout" in paths}
'''), {"status": 200, "has_books": True, "has_blurb": True, "has_checkout": True}, everyday=True))

CASES.append(Case("MCP", "Claude Code style session: initialize, list, call", textwrap.dedent('''
with serve(BOOKSTORE, env=ENV) as s:
    init = s.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "probe", "version": "1"}}, headers=KEY)
    tools = s.rpc("tools/list", {}, rid=2, headers=KEY)
    names = sorted(t["name"] for t in tools["result"]["tools"])
    s.post("/books", {"title": "Dune", "author": "Herbert", "year": 1965}, headers=KEY)
    call = s.rpc("tools/call", {"name": "list_books", "arguments": {}}, rid=3, headers=KEY)
    sc = call["result"].get("structuredContent")
    result = {"server_name": init["result"]["serverInfo"]["name"],
              "has_list": "list_books" in names, "has_blurb": any("blurb" in n for n in names),
              "structured_is_object": isinstance(sc, dict), "is_error": call["result"].get("isError", False)}
'''), {"server_name": "bookstore", "has_list": True, "has_blurb": True, "structured_is_object": True,
       "is_error": False}, everyday=True))

CASES.append(Case("Agents", "agent calls a tool through the fake provider", textwrap.dedent('''
src = BOOKSTORE + """
app.agent("librarian", description="Finds books.", tools=["list_books"], scopes=["read"], token_budget=60_000)
"""
with serve(src, env=ENV) as s:
    s.post("/books", {"title": "Dune", "author": "Herbert", "year": 1965}, headers=KEY)
    st, _, run = s.post("/agents/librarian", {"input": "tool:list_books {}"}, headers=KEY)
    result = {"status": st, "run_status": run.get("status"), "steps_have_tool": any(
        (step.get("tool") or step.get("name")) == "list_books" for step in run.get("steps", [])),
        "mentions_dune": "Dune" in json.dumps(run), "tokens_counted": (run.get("usage") or {}).get("input_tokens", 0) > 0}
'''), {"status": 200, "run_status": "completed", "steps_have_tool": True, "mentions_dune": True,
       "tokens_counted": True}, everyday=True))

CASES.append(Case("Agents", "handoff from front desk to specialist", textwrap.dedent('''
src = BOOKSTORE + """
app.agent("librarian", description="Finds books.", tools=["list_books"], scopes=["read"])
app.agent("front_desk", handoffs=["librarian"], scopes=["read"])
"""
with serve(src, env=ENV) as s:
    st, _, run = s.post("/agents/front-desk", {"input": "handoff:librarian"}, headers=KEY)
    result = {"status": st, "run_status": run.get("status"), "handed_to_librarian": "librarian" in json.dumps(run)}
'''), {"status": 200, "run_status": "completed", "handed_to_librarian": True}, everyday=True))

CASES.append(Case("Agents", "behaviour fans out calls and returns usage", textwrap.dedent('''
src = BOOKSTORE + """
@app.behaviour("count_books", tools=["list_books"], scopes=["read"], token_budget=50_000)
def count_books(ctx, input):
    books = ctx.call("list_books")
    echoes = ctx.ask_many([f"echo {b['title']}" for b in books])
    return {"n": len(books), "echoes": len(echoes)}
"""
with serve(src, env=ENV) as s:
    for t in ("A", "B", "C"):
        s.post("/books", {"title": t, "author": "x", "year": 2000}, headers=KEY)
    st, _, run = s.post("/behaviours/count-books", {"input": {}}, headers=KEY)
    result = {"status": st, "output": run}
'''), {"status": 200, "output": {"n": 3, "echoes": 3}}, everyday=True))

CASES.append(Case("Agents", "pipeline flow runs both steps", textwrap.dedent('''
src = BOOKSTORE + """
app.agent("a", scopes=["read"])
app.agent("b", scopes=["read"])
app.flow("ab", pipeline=["a", "b"], scopes=["read"])
"""
with serve(src, env=ENV) as s:
    st, _, run = s.post("/flows/ab", {"input": "hello"}, headers=KEY)
    result = {"status": st, "run_status": run.get("status"), "two_steps": len(run.get("steps", [])) >= 2}
'''), {"status": 200, "run_status": "completed", "two_steps": True}, everyday=True))

CASES.append(Case("Agents", "provider failure is a coded error, server stays up", textwrap.dedent('''
src = BOOKSTORE + """
app.agent("librarian", scopes=["read"])
"""
with serve(src, env=ENV) as s:
    st, hdrs, run = s.post("/agents/librarian", {"input": "fail:upstream exploded"}, headers=KEY)
    result = {"status": st, "coded": bool(hdrs.get("x-webcortex-error")), "alive": s.alive()}
'''), {"status": 502, "coded": True, "alive": True}, everyday=True))

CASES.append(Case("Devices", "camera pushes frames over WebSocket, snapshot serves the latest", textwrap.dedent('''
src = """
from webcortex import WebCortex
app = WebCortex("hub")
app.anonymous_scopes("devices:ingest", "devices:read")
app.camera("dock", max_fps=1000)
"""
with serve(src) as s:
    ws = s.ws("/devices/dock/ws")
    png = bytes.fromhex("89504e470d0a1a0a") + b"frame-1"
    ws.send(png)
    time.sleep(0.1)                      # max_fps drops a frame that arrives too soon
    ws.send(bytes.fromhex("89504e470d0a1a0a") + b"frame-2")
    time.sleep(0.5)
    st, hdrs, body = s.get("/devices/dock/snapshot")
    ws.close()
    import base64
    frame = base64.b64decode(body["frame"]["$image"]["data"]) if isinstance(body, dict) else b""
    result = {"upgrade": ws.status, "snapshot": st, "latest": frame.endswith(b"frame-2"), "seq": body.get("seq")}
'''), {"upgrade": 101, "snapshot": 200, "latest": True, "seq": 2}, everyday=True))

CASES.append(Case("CLI", "doctor and context run on a fresh starter", textwrap.dedent('''
import tempfile, pathlib
d = pathlib.Path(tempfile.mkdtemp())
rc_new, out, err = cli("new", "shop", cwd=d)
proj = d / "shop"
rc_ctx, ctx_out, _ = cli("context", cwd=proj)
rc_doc, doc_out, doc_err = cli("doctor", cwd=proj, env={"WEBCORTEX_FAKE_PROVIDER": "1"})
result = {"new": rc_new, "api_exists": (proj / "api.py").exists(), "context_ok": rc_ctx == 0 and len(ctx_out) > 200,
          "doctor_ran": "Traceback" not in doc_out + doc_err}
'''), {"new": 0, "api_exists": True, "context_ok": True, "doctor_ran": True}, everyday=True))


# ---------------------------------------------------------------------------
# Edge cases fixed for 2.6.0 — `expect` is the correct behaviour, so 2.5.3
# should fail each of these and the dev tree pass it.
# ---------------------------------------------------------------------------

# A scriptable OpenAI-compatible model server, started inside a snippet.
MODEL_STUB = r'''
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class ModelStub:
    """Answers POSTs from `script` (status, body, headers) in order; 500 when empty."""
    def __init__(self, *script):
        self.script, self.seen, lock, stub = list(script), [], threading.Lock(), self
        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass
            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("content-length") or 0))
                with lock:
                    stub.seen.append(raw)
                    status, body, headers = stub.script.pop(0) if stub.script else (500, {"error": "empty"}, {})
                data = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(status)
                for k, v in headers.items():
                    self.send_header(k, v)
                if "content-type" not in headers:
                    self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.httpd.server_address[1]
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

def oa_text(text, finish="stop"):
    return {"model": "m", "choices": [{"finish_reason": finish, "message": {"role": "assistant", "content": text}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}}

def oa_call(name, arguments, finish="tool_calls"):
    return {"model": "m", "choices": [{"finish_reason": finish, "message": {"role": "assistant", "content": None,
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": name, "arguments": arguments}}]}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}}

def wire_app(port):
    return f"""
from webcortex import WebCortex
app = WebCortex("wire")
app.provider("stub", base_url="http://127.0.0.1:{port}/v1")
CALLS = []

@app.post("/record", tool=True, tool_name="record")
def record(note: str = "") -> dict:
    CALLS.append(note)
    return {{"recorded": note}}

@app.get("/recorded")
def recorded() -> dict:
    return {{"calls": list(CALLS)}}

app.agent("oa", model="stub/m", tools=["record"], max_steps=4)
"""
'''

PNG_HEX = ("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154"
           "78da63646060f80f0002870180eb47ba920000000049454e44ae426082")


def edge(section, label, expect, why):
    return case(section, label, expect, why=why)


# -- HTTP --------------------------------------------------------------------

edge("HTTP", "HEAD on a GET route (curl -I, load balancer checks)",
     {"get": 200, "head": 200, "head_body_empty": True, "length_matches": True},
     "2.5.3 answered 405 to HEAD on every route, so `curl -I` and LB health checks failed.")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("h")
app.static("GET", "/ping", {"pong": True})
@app.get("/hello")
def hello() -> dict:
    return {"hi": "there"}
"""
import urllib.request, urllib.error
with serve(src) as s:
    g = urllib.request.urlopen(s.base + "/hello", timeout=10)
    gbody = g.read()
    try:
        h = urllib.request.urlopen(urllib.request.Request(s.base + "/hello", method="HEAD"), timeout=10)
        hst, hbody, hlen = h.status, h.read(), h.headers.get("content-length")
    except urllib.error.HTTPError as e:
        hst, hbody, hlen = e.code, b"", None
    result = {"get": g.status, "head": hst, "head_body_empty": hbody == b"",
              "length_matches": hlen == str(len(gbody))}
''')

edge("HTTP", "SIGTERM lets an in-flight slow request finish",
     {"status": 200, "body": {"finished": True}, "exit_code": 0},
     "2.5.3's drain never waited (a u32 permit truncation), so a request in flight at SIGTERM was cut off.")(r'''
import signal, threading, http.client
src = """
import time
from pathlib import Path
from webcortex import WebCortex
app = WebCortex("draining", request_timeout=30, shutdown_timeout=20)
@app.get("/work")
def work() -> dict:
    Path("started").write_text("1")
    time.sleep(2.0)
    return {"finished": True}
"""
with serve(src) as s:
    outcome = {}
    def in_flight():
        conn = http.client.HTTPConnection("127.0.0.1", s.port, timeout=30)
        try:
            conn.request("GET", "/work")
            r = conn.getresponse()
            outcome["status"], outcome["body"] = r.status, json.loads(r.read())
        except Exception as e:
            outcome["status"] = repr(e)[:80]
    t = threading.Thread(target=in_flight)
    t.start()
    deadline = time.time() + 20
    while not (s.workdir / "started").exists() and time.time() < deadline:
        time.sleep(0.02)
    s.proc.send_signal(signal.SIGTERM)
    t.join(30)
    try:
        code = s.proc.wait(timeout=25)
    except Exception:
        code = "still running"
    result = {"status": outcome.get("status"), "body": outcome.get("body"), "exit_code": code}
''')

edge("HTTP", "static_files serves its index at the mount point",
     {"slash": [200, "<h1>root index</h1>"], "bare": [200, "<h1>root index</h1>"]},
     "2.5.3 answered 404 at /assets/ and /assets because the catch-all never matched an empty rest.")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("site")
app.static_files("/assets", "static", index="index.html")
"""
with serve(src, files={"static/index.html": "<h1>root index</h1>", "static/a.txt": "hello"}) as s:
    a = s.get("/assets/")
    b = s.get("/assets")
    result = {"slash": [a[0], a[2]], "bare": [b[0], b[2]]}
''')

edge("HTTP", "query param 007 keeps its leading zero",
     {"zip": {"t": "text", "v": "007"}, "plain_int": {"t": "integer", "v": 12}},
     "2.5.3 bound numeric-looking query text as a number, so 007 became 7 (and nan/inf became NULL).")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("q", database="sqlite://./q.db")
app.query("GET", "/echo", "SELECT ? AS v, typeof(?) AS t", params=["v", "v"], returns="one")
"""
with serve(src) as s:
    result = {"zip": s.get("/echo?v=007")[2], "plain_int": s.get("/echo?v=12")[2]}
''')

edge("HTTP", "malformed JSON POST to a resource is 400 and inserts nothing",
     {"status": 400, "code": "WC205", "rows_added": 0},
     "2.5.3 treated an unreadable body as absent and inserted a row of NULLs.")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("r", database="sqlite://./r.db")
app.resource("items", fields={"id": int, "name": str}, scopes=[])  # public on purpose
"""
with serve(src) as s:
    before = len(s.get("/items")[2])
    st, _, body = s.post("/items", b"{bad json", headers={"content-type": "application/json"})
    after = len(s.get("/items")[2])
    result = {"status": st, "code": (body or {}).get("error", {}).get("code") if isinstance(body, dict) else body,
              "rows_added": after - before}
''')

edge("HTTP", ".html.j2 template is autoescaped",
     {"status": 200, "escaped": True, "raw_script": False},
     "2.5.3 autoescaped only names ending .html/.htm/.xml, so page.html.j2 rendered <script> verbatim (XSS).")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("p", templates="templates")
app.page("/page", "page.html.j2", data={"x": "<script>alert(1)</script>"})
"""
with serve(src, files={"templates/page.html.j2": "<p>{{ data.x }}</p>"}) as s:
    st, _, body = s.get("/page")
    text = body if isinstance(body, str) else json.dumps(body)
    result = {"status": st, "escaped": "&lt;script&gt;" in text, "raw_script": "<script>" in text}
''')

edge("HTTP", "a handler's Content-Length cannot split the response stream",
     {"first_status": 200, "two_responses": True, "first_body_intact": True, "ping_after": True},
     "2.5.3 passed a handler's Content-Length/Transfer-Encoding to the wire as given, breaking the framing: here the client got no response at all.")(r'''
import re
src = """
from webcortex import WebCortex, Response
app = WebCortex("frame")
app.static("GET", "/ping", {"pong": True})
@app.get("/wrong-length")
def wrong_length():
    return Response({"payload": "HTTP/1.1 200 OK"}, headers={"Content-Length": "2", "Transfer-Encoding": "chunked", "Connection": "keep-alive"})
"""
with serve(src) as s:
    reply = s.raw(b"GET /wrong-length HTTP/1.1\r\nHost: x\r\n\r\n"
                  b"GET /ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
    head, _, rest = reply.partition(b"\r\n\r\n")
    m = re.match(rb"HTTP/1\.1 (\d{3})", reply)
    result = {"first_status": int(m.group(1)) if m else None,
              "two_responses": reply.count(b"x-request-id: ") == 2,
              "first_body_intact": rest.startswith(b'{"payload": "HTTP/1.1 200 OK"}'),
              "ping_after": reply.endswith(b'{"pong":true}')}
''')

edge("HTTP", "non-ASCII x-api-key is refused, not served as anonymous",
     {"no_key": 200, "garbled_key": 401},
     "2.5.3 silently dropped a non-ASCII header value, so a garbled credential was served as an anonymous caller.")(r'''
import re
src = """
from webcortex import WebCortex
app = WebCortex("k")
app.api_key("SVC_KEY", id="svc", scopes=["open", "admin"])
app.anonymous_scopes("open")
app.static("GET", "/open", {"ok": True}, scopes=["open"])
"""
with serve(src, env={"SVC_KEY": "svc-key-0123456789abcdef0123456789"}) as s:
    reply = s.raw(b"GET /open HTTP/1.1\r\nHost: x\r\nx-api-key: \xffnot-a-key\r\nConnection: close\r\n\r\n")
    m = re.match(rb"HTTP/1\.[01] (\d{3})", reply)
    result = {"no_key": s.get("/open")[0], "garbled_key": int(m.group(1)) if m else None}
''')

edge("HTTP", "a signed JWT `root` claim cannot impersonate another user",
     {"status": 200, "who": "mallory"},
     "2.5.3 took a token's `root` claim as the caller's root identity, so @principal bound to whoever it named.")(r'''
import base64, hashlib, hmac
SECRET = "workflow-jwt-secret-that-is-at-least-32-bytes"
def b64(d): return base64.urlsafe_b64encode(d).rstrip(b"=").decode()
def jwt(claims):
    h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode()); b = b64(json.dumps(claims).encode())
    return f"{h}.{b}." + b64(hmac.new(SECRET.encode(), f"{h}.{b}".encode(), hashlib.sha256).digest())
src = """
from webcortex import WebCortex
app = WebCortex("j", database="sqlite://./j.db")
app.jwt(secret_env="JWT_SECRET")
app.query("GET", "/mine", "SELECT ? AS who", params=["@principal"], returns="one")
"""
with serve(src, env={"JWT_SECRET": SECRET}) as s:
    tok = jwt({"sub": "mallory", "root": "alice", "exp": int(time.time()) + 300})
    st, _, body = s.get("/mine", headers={"authorization": f"Bearer {tok}"})
    result = {"status": st, "who": body.get("who") if isinstance(body, dict) else body}
''')

# -- Agents / MCP ------------------------------------------------------------

edge("Agents", "provider 429 (text body) is retried and the run completes",
     {"status": 200, "run_status": "completed", "output": "hello", "upstream_calls": 2},
     "2.5.3 parsed the body before the status, so a 429 answered in plain text failed as WC305 instead of being retried.")(MODEL_STUB + r'''
stub = ModelStub((429, b"slow down", {"content-type": "text/plain", "retry-after": "0"}), (200, oa_text("hello"), {}))
with serve(wire_app(stub.port), fake=False) as s:
    st, _, run = s.post("/agents/oa", {"input": "hi"}, timeout=60)
    result = {"status": st, "run_status": run.get("status"), "output": run.get("output"),
              "upstream_calls": len(stub.seen)}
''')

edge("Agents", "tool calls under finish_reason 'stop' (Ollama) are run",
     {"run_status": "completed", "output": "done", "tool_ran": ["stop"]},
     "2.5.3 ended the run at finish_reason 'stop' even with tool_calls present: 'completed', no output, tool never run.")(MODEL_STUB + r'''
stub = ModelStub((200, oa_call("record", '{"note": "stop"}', finish="stop"), {}), (200, oa_text("done"), {}))
with serve(wire_app(stub.port), fake=False) as s:
    st, _, run = s.post("/agents/oa", {"input": "hi"}, timeout=60)
    result = {"run_status": run.get("status"), "output": run.get("output"),
              "tool_ran": s.get("/recorded")[2]["calls"]}
''')

edge("Agents", "garbled tool-call arguments are refused (WC317), tool not run",
     {"run_status": "completed", "tool_ran": [], "refused_code": "WC317"},
     "2.5.3 ran the tool with {} when the model's argument string did not parse.")(MODEL_STUB + r'''
stub = ModelStub((200, oa_call("record", '{"note": "half'), {}), (200, oa_text("ok"), {}))
with serve(wire_app(stub.port), fake=False) as s:
    st, _, run = s.post("/agents/oa", {"input": "hi"}, timeout=60)
    refused = [x for x in run.get("steps", []) if x.get("kind") == "tool_refused"]
    result = {"run_status": run.get("status"), "tool_ran": s.get("/recorded")[2]["calls"],
              "refused_code": refused[0].get("code") if refused else None}
''')

edge("MCP", "batch with non-object items and a bad id gets -32600 answers",
     {"status": 200, "answers": 3, "codes": [-32600, -32600], "ping_ok": True, "bad_id_code": -32600},
     "2.5.3 silently dropped invalid batch members and accepted an object as a request id.")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("m")
app.static("GET", "/ping", {"pong": True}, tool=True, tool_name="ping")
"""
with serve(src) as s:
    st, _, out = s.post("/_webcortex/mcp", [1, "x", {"jsonrpc": "2.0", "id": 2, "method": "ping"}])
    out = out if isinstance(out, list) else [out]
    _, _, bad = s.post("/_webcortex/mcp", {"jsonrpc": "2.0", "id": {"nested": 1}, "method": "ping"})
    result = {"status": st, "answers": len(out),
              "codes": [(o or {}).get("error", {}).get("code") for o in out[:2] if isinstance(o, dict)],
              "ping_ok": isinstance(out[-1], dict) and out[-1].get("result") == {},
              "bad_id_code": ((bad or {}).get("error") or {}).get("code") if isinstance(bad, dict) else bad}
''')

edge("Agents", "a behaviour that swallows its halt cannot keep spending",
     {"status": 200, "bounded": True},
     "2.5.3 checked the token budget only after each call, so `except: continue` made (and paid for) all fifty calls.")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("b")

@app.behaviour("stubborn", tools=[], token_budget=25, max_steps=200)
def stubborn(ctx, input):
    for _ in range(50):
        try:
            ctx.ask("again")
        except Exception as e:
            if type(e).__name__ != "BehaviourHalted":
                raise
    return {"done": True}
"""
with serve(src) as s:
    st, _, out = s.post("/behaviours/stubborn", {})
    usage = s.get("/_webcortex/usage")[2]
    calls = usage["by_caller"].get("behaviour:stubborn", {}).get("calls", 0)
    result = {"status": st, "calls": calls, "bounded": calls <= 3}
''')

# -- Python ------------------------------------------------------------------

ODD_APP = '''
import asyncio, datetime, enum, math
from webcortex import WebCortex, Response
app = WebCortex("py", request_timeout=3, workers=1)

class Colour(enum.Enum):
    RED = "red"

@app.get("/odd")
def odd() -> dict:
    return {"nan": math.nan, "inf": [math.inf, 1.5], "colour": Colour.RED,
            "when": datetime.datetime(2024, 1, 2, 3, 4)}

@app.post("/echo")
def echo(text: str, n: int = 0) -> dict:
    return {"text": text, "n": n}

@app.get("/bad-status")
def bad_status() -> Response:
    return Response({"x": 1}, status=1000)

@app.get("/cyclic")
def cyclic() -> dict:
    d = {"a": 1}
    d["self"] = d
    return d

@app.get("/exit")
async def exits() -> dict:
    raise SystemExit(2)

@app.get("/async")
async def asynch(i: int = 0) -> dict:
    await asyncio.sleep(0.001)
    return {"i": i}

@app.get("/sync")
def sync() -> dict:
    return {"ok": True}

@app.behaviour("cyclic_result")
def cyclic_result(ctx, input):
    d = {"a": 1}
    d["self"] = d
    return d
'''

edge("Python", "NaN/inf in a return value is valid JSON null",
     {"strict_json": True, "nan": None, "inf": [None, 1.5]},
     "2.5.3 wrote the bare tokens NaN/Infinity, which JSON.parse in every browser rejects.")(r'''
import urllib.request
with serve(ODD_APP_SRC) as s:
    raw = urllib.request.urlopen(s.base + "/odd", timeout=10).read()
    def strict(c): raise ValueError(c)
    try:
        body, ok = json.loads(raw, parse_constant=strict), True
    except ValueError:
        body, ok = json.loads(raw), False
    result = {"strict_json": ok, "nan": body.get("nan"), "inf": body.get("inf")}
''')

edge("Python", "an Enum return encodes as its value",
     {"colour": "red"},
     "2.5.3 encoded an Enum through vars(), dumping the class dict (module names, function addresses).")(r'''
with serve(ODD_APP_SRC) as s:
    result = {"colour": s.get("/odd")[2].get("colour")}
''')

edge("Python", "a datetime return is ISO 8601 with a T",
     {"when": "2024-01-02T03:04:00"},
     "2.5.3 encoded datetimes with str(), a space instead of the T that `format: date-time` promises.")(r'''
with serve(ODD_APP_SRC) as s:
    result = {"when": s.get("/odd")[2].get("when")}
''')

edge("Python", "a non-JSON body to a Python handler is 400 WC205",
     {"status": 400, "code": "WC205"},
     "2.5.3 failed the request as a 500 handler error when the body was not JSON.")(r'''
with serve(ODD_APP_SRC) as s:
    st, _, body = s.post("/echo", b"{bad", headers={"content-type": "application/json"})
    result = {"status": st, "code": body.get("error", {}).get("code") if isinstance(body, dict) else body}
''')

edge("Python", "an invalid status from a handler is a coded JSON 500",
     {"status": 500, "code": "WC002"},
     "2.5.3 answered an unsendable handler response with a bare text/plain \"response build failed\" carrying no error code.")(r'''
with serve(ODD_APP_SRC) as s:
    st, _, body = s.get("/bad-status")
    result = {"status": st, "code": body.get("error", {}).get("code") if isinstance(body, dict) else body}
''')

edge("Python", "a handler returning a cyclic dict leaks no traceback",
     {"status": 500, "code": "WC002", "traceback_free": True},
     "2.5.3 put the full Python traceback (file paths, source lines) in the 500 body sent to the client.")(r'''
with serve(ODD_APP_SRC) as s:
    st, _, body = s.get("/cyclic")
    text = json.dumps(body)
    result = {"status": st, "code": body.get("error", {}).get("code") if isinstance(body, dict) else body,
              "traceback_free": "Traceback" not in text and 'File \\"' not in text}
''')

edge("Python", "sys.exit() in an async handler, then 10 requests all succeed",
     {"exit_status": 500, "all_ok": True, "fast": True},
     "2.5.3 let SystemExit kill the event-loop thread, so every later async request on it timed out with 504.")(r'''
with serve(ODD_APP_SRC) as s:
    first = s.get("/exit")[0]
    started, ok = time.time(), True
    for i in range(10):
        try:
            st, _, body = s.get(f"/async?i={i}", timeout=8)
        except Exception:
            st, body = None, None
        if (st, body) != (200, {"i": i}):
            ok = False
            break
    result = {"exit_status": first, "all_ok": ok, "fast": time.time() - started < 8}
''')

edge("Python", "a behaviour returning a cyclic dict fails cleanly, server lives",
     {"status": 500, "code": "WC003", "alive": True},
     "2.5.3 recursed on the cycle in the Rust converter until the stack overflowed and the server process died.")(r'''
with serve(ODD_APP_SRC) as s:
    try:
        st, _, body = s.post("/behaviours/cyclic-result", {}, timeout=15)
        code = body.get("error", {}).get("code") if isinstance(body, dict) else body
    except Exception as e:
        st, code = repr(e)[:60], None
    time.sleep(0.3)
    result = {"status": st, "code": code, "alive": s.alive()}
''')

edge("Python", "a resource with a str primary key round-trips",
     {"create": 200, "get": [200, "Red"], "update": 200, "delete_then_get": 404},
     "2.5.3 left a non-INTEGER key out of the INSERT, so every row got a NULL key that could never be fetched.")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("tags", database="sqlite://./tags.db")
app.resource("tags", fields={"slug": str, "label": str}, primary_key="slug", scopes=[])  # public on purpose
"""
with serve(src) as s:
    c = s.post("/tags", {"slug": "red", "label": "Red"})[0]
    g = s.get("/tags/red")
    u = s.request("PATCH", "/tags/red", {"label": "Crimson"})[0]
    if u == 405:
        u = s.request("PUT", "/tags/red", {"label": "Crimson"})[0]
    s.request("DELETE", "/tags/red")
    result = {"create": c, "get": [g[0], g[2].get("label") if isinstance(g[2], dict) else g[2]],
              "update": u, "delete_then_get": s.get("/tags/red")[0]}
''')

edge("CLI", "`webcortex new` with quotes in the name gives an importable api.py",
     {"new": 0, "parses": True, "check": 0},
     "2.5.3 pasted the name into api.py unescaped, so a quote produced a SyntaxError in the fresh project.")(r'''
import ast, tempfile, pathlib
d = pathlib.Path(tempfile.mkdtemp())
rc, out, err = cli("new", 'my "best" app', "-d", "proj", cwd=d)
api = d / "proj" / "api.py"
try:
    ast.parse(api.read_text())
    parses = True
except Exception:
    parses = False
keys = {v: "test-key-abcdefghijklmnop" for v in ("WEBCORTEX_API_KEY", "WEBCORTEX_VIEWER_KEY",
                                                 "WEBCORTEX_DEVICE_KEY", "WEBCORTEX_READER_KEY")}
rc_check = cli("check", cwd=d / "proj", env=keys)[0] if api.exists() else None
result = {"new": rc, "parses": parses, "check": rc_check}
''')

# -- Devices / Safety --------------------------------------------------------

edge("Safety", "halt while actuator calls are queued: queued ones get 423",
     {"halt": 200, "ran_at_most_4": True, "refused_at_least_8": True, "moves_match": True},
     "2.5.3 checked the e-stop only at dispatch, so actuator calls queued behind busy threads still moved after the stop.")(r'''
import threading
src = """
import time
from webcortex import WebCortex
app = WebCortex("arm", workers=1, request_timeout=30)
STARTED, MOVES = [], []

@app.post("/move", actuator=True)
def move(n: int, secs: float = 0.5) -> dict:
    STARTED.append(n)
    time.sleep(secs)
    MOVES.append(n)
    return {"moved": n}

@app.get("/moves")
async def moves() -> dict:
    return {"started": STARTED, "moves": MOVES}
"""
with serve(src) as s:
    results = {}
    def mover(i):
        try:
            results[i] = s.post("/move", {"n": i, "secs": 0.5}, timeout=60)[0]
        except Exception as e:
            results[i] = repr(e)[:40]
    threads = [threading.Thread(target=mover, args=(i,)) for i in range(12)]
    for t in threads:
        t.start()
    deadline = time.time() + 20
    while len(s.get("/moves")[2]["started"]) < 4 and time.time() < deadline:
        time.sleep(0.02)
    halt = s.post("/_webcortex/halt", {"reason": "workflow test"})[0]
    for t in threads:
        t.join(60)
    codes = list(results.values())
    moved = s.get("/moves")[2]["moves"]
    s.post("/_webcortex/release", {})
    result = {"halt": halt, "codes": sorted(map(str, codes)), "ran_at_most_4": codes.count(200) <= 4,
              "refused_at_least_8": codes.count(423) >= 8, "moves_match": len(moved) == codes.count(200)}
''')

edge("Devices", "10 fps camera with +-10 ms timer jitter keeps its frames",
     {"upgrade": 101, "dropped_ok": True},
     "2.5.3 refused any frame less than a full interval after the last, so a 10 fps camera running a few ms early lost about half.")(r'''
src = """
from webcortex import WebCortex
app = WebCortex("hub")
app.anonymous_scopes("devices:ingest", "devices:read")
app.camera("paced", max_fps=10)
"""
png = bytes.fromhex(PNG_HEX)
def dev(s):
    return next(d for d in s.get("/_webcortex/devices")[2]["devices"] if d["name"] == "paced")
with serve(src) as s:
    ws = s.ws("/devices/paced/ws")
    last = time.monotonic()
    for k in range(20):
        target = last + 0.1 - (0.01 if k % 2 else 0.0)   # up to 10 ms early
        time.sleep(max(0.0, target - time.monotonic()))
        ws.send(png)
        last = time.monotonic()
    time.sleep(0.5)
    d = dev(s)
    ws.close()
    result = {"upgrade": ws.status, "dropped": d["frames_dropped"], "in": d["frames_in"],
              "dropped_ok": d["frames_dropped"] <= 3}
''')

edge("Devices", "a device that never reads its error replies keeps being processed",
     {"send_blocked": False, "all_rejected_counted": True, "good_frame_in": True},
     "2.5.3 wrote every error reply inline; once the unread replies filled the socket the hub stopped reading and the device blocked.")(r'''
import socket
src = """
from webcortex import WebCortex
app = WebCortex("hub")
app.anonymous_scopes("devices:ingest", "devices:read")
app.camera("dock", max_fps=1000, keep=4)
"""
png = bytes.fromhex(PNG_HEX)
def dev(s):
    return next(d for d in s.get("/_webcortex/devices")[2]["devices"] if d["name"] == "dock")
with serve(src) as s:
    ws = s.ws("/devices/dock/ws", timeout=15)
    ws.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    bad = b"BM not a supported image"
    key = b"\x01\x02\x03\x04"
    frame = bytes([0x82, 0x80 | len(bad)]) + key + bytes(b ^ key[i % 4] for i, b in enumerate(bad))
    blocked = False
    try:
        ws.sock.sendall(frame * 20000)
        ws.send(png)
    except (socket.timeout, OSError):
        blocked = True
    deadline = time.time() + 30
    d = dev(s)
    while time.time() < deadline and not (d["frames_rejected"] >= 20000 and d["frames_in"] >= 1):
        time.sleep(0.2)
        d = dev(s)
    ws.close()
    result = {"send_blocked": blocked, "rejected": d["frames_rejected"],
              "all_rejected_counted": d["frames_rejected"] >= 20000, "good_frame_in": d["frames_in"] >= 1}
''')

edge("Devices", "a WebSocket protocol violation gets close code 1002",
     {"upgrade": 101, "ready_first": True, "close_code": 1002},
     "2.5.3 dropped the connection with no close frame, so the client saw 1006 and no reason.")(r'''
import struct
src = """
from webcortex import WebCortex
app = WebCortex("hub")
app.anonymous_scopes("devices:ingest", "devices:read")
app.sensor("temp", max_fps=1000)
"""
with serve(src) as s:
    ws = s.ws("/devices/temp/ws")
    ws.send(b"{}", opcode=1, mask=False)          # clients must mask every frame
    frames = []
    for _ in range(5):
        op, data = ws.recv(timeout=5)
        if op is None:
            break
        frames.append((op, data))
        if op == 8:
            break
    ws.close()
    closes = [d for op, d in frames if op == 8]
    result = {"upgrade": ws.status, "ready_first": bool(frames) and frames[0][0] == 1 and b"ready" in frames[0][1],
              "close_code": struct.unpack(">H", closes[0][:2])[0] if closes and len(closes[0]) >= 2 else None}
''')

edge("Devices", "Image(b'junk', 'image/png') is refused",
     {"junk_refused": True, "mislabelled_jpeg_refused": True, "real_png_ok": "image/png"},
     "2.5.3 trusted the media-type label, so junk bytes labelled image/png were sent to the model as an image.")(r'''
from webcortex import Image
def refused(*a):
    try:
        Image(*a)
        return False
    except ValueError:
        return True
result = {"junk_refused": refused(b"junk", "image/png"),
          "mislabelled_jpeg_refused": refused(b"\xff\xd8\xff\xe0" + b"\0" * 16, "image/png"),
          "real_png_ok": Image(bytes.fromhex(PNG_HEX)).media_type}
''')

edge("Devices", "concurrent snapshots of a pulled IP camera share one fetch",
     {"statuses": [200] * 10, "shared": True},
     "2.5.3 fetched the IP camera once per asker: ten snapshots meant ten connections to one slow camera.")(r'''
import threading, http.server
png = bytes.fromhex(PNG_HEX)
hits = []
class Cam(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def do_GET(self):
        hits.append(1)
        time.sleep(1.0)
        self.send_response(200)
        self.send_header("content-type", "image/png")
        self.send_header("content-length", str(len(png)))
        self.end_headers()
        self.wfile.write(png)
cam = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Cam)
threading.Thread(target=cam.serve_forever, daemon=True).start()
src = f"""
from webcortex import WebCortex
app = WebCortex("hub")
app.anonymous_scopes("devices:read")
app.camera("ipcam", source="http://127.0.0.1:{cam.server_address[1]}/snapshot.png")
"""
with serve(src) as s:
    gate = threading.Barrier(10, timeout=20)
    statuses = []
    def snap():
        gate.wait()
        try:
            statuses.append(s.get("/devices/ipcam/snapshot", timeout=30)[0])
        except Exception as e:
            statuses.append(repr(e)[:40])
    threads = [threading.Thread(target=snap) for _ in range(10)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(40)
    result = {"statuses": statuses, "fetches": len(hits), "shared": len(hits) <= 2}
''')


# Throughput of one app under each build: a Rust-executed static route, a Rust
# SQL route and a Python handler, over keep-alive connections. Interleaved
# rounds, best of three, because machine load moves these numbers by ±20%.
TIMING = textwrap.dedent('''
src = """
from webcortex import WebCortex
app = WebCortex("bench", database="sqlite://./b.db")
app.static("GET", "/ping", {"pong": True})
app.resource("items", fields={"id": int, "name": str})
@app.get("/py")
def py() -> dict:
    return {"ok": True}
"""
with serve(src, fake=False) as s:
    s.post("/items", {"name": "a"})
    result = {}
    for path in ("/ping", "/items", "/py"):
        throughput(s.port, path, seconds=0.5)          # warm up
        result[path] = max(throughput(s.port, path, seconds=2.0) for _ in range(3))
''')


def _source(case: Case) -> str:
    prelude = "import json, time\nfrom harness import serve, cli, throughput\n"
    consts = (f"BOOKSTORE = {BOOKSTORE!r}\nKEY = {KEY!r}\nENV = {ENV!r}\n"
              f"ODD_APP_SRC = {ODD_APP!r}\nPNG_HEX = {PNG_HEX!r}\n")
    return prelude + consts + case.code + "\nprint('@@RESULT@@' + json.dumps(result, default=repr))\n"


def run_case(case: Case, pythonpath: str, timeout: float = 180.0) -> Dict[str, Any]:
    """Run one case in a fresh interpreter with ``pythonpath`` first on the path."""
    env = {**os.environ, "PYTHONPATH": os.pathsep.join([pythonpath, str(HERE)])}
    env.pop("WEBCORTEX_FAKE_PROVIDER", None)
    started = time.perf_counter()
    try:
        p = subprocess.run([sys.executable, "-c", _source(case)], env=env, capture_output=True, text=True,
                           timeout=timeout, cwd=str(Path.home()))
    except subprocess.TimeoutExpired:
        return {"error": f"timed out after {timeout:.0f}s", "seconds": timeout}
    seconds = time.perf_counter() - started
    for line in p.stdout.splitlines():
        if line.startswith("@@RESULT@@"):
            return {"value": json.loads(line[len("@@RESULT@@"):]), "seconds": seconds}
    tail = (p.stderr or p.stdout).strip().splitlines()[-1:] or ["no output"]
    return {"error": tail[0][:300], "seconds": seconds}


def judge(case: Case, outcome: Dict[str, Any]) -> Dict[str, Any]:
    if "error" in outcome:
        return {"ok": False, "why": outcome["error"]}
    value = outcome["value"]
    wrong = {k: value.get(k, "<missing>") for k, want in case.expect.items() if value.get(k, "<missing>") != want}
    return {"ok": not wrong, "why": "" if not wrong else "; ".join(f"{k}={v!r} (want {case.expect[k]!r})"
                                                                   for k, v in wrong.items())}


def baseline_path(version: str = BASELINE_VERSION) -> str:
    """Install the published wheel once into ~/.cache and return its directory."""
    target = Path.home() / ".cache" / "webcortex-baseline" / version
    if not (target / "webcortex").exists():
        subprocess.run(["uv", "pip", "install", "-q", "--no-deps", "--python", sys.executable,
                        "--target", str(target), f"web-cortex-framework=={version}"], check=True)
    return str(target)


def dev_path() -> str:
    return str(REPO / "python")


def main(argv: Optional[List[str]] = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", choices=["both", "dev", "baseline"], default="both")
    ap.add_argument("-k", default="")
    args = ap.parse_args(argv)
    builds = {"baseline": baseline_path, "dev": dev_path}
    chosen = ["baseline", "dev"] if args.build == "both" else [args.build]
    fails = {b: 0 for b in chosen}
    for c in CASES:
        if args.k and args.k.lower() not in (c.section + " " + c.label).lower():
            continue
        cells = []
        for b in chosen:
            v = judge(c, run_case(c, builds[b]()))
            fails[b] += not v["ok"]
            cells.append(("ok  " if v["ok"] else "FAIL") + ("" if v["ok"] else f" {v['why'][:70]}"))
        print(f"{c.section:8} {c.label[:52]:52} | " + " | ".join(cells))
    print("fails:", fails)
    return 0


if __name__ == "__main__":
    sys.exit(main())
