"""Score how well WebCortex reports realistic failures.

    python benchmarks/diagnostics/run.py --python /path/to/venv/bin/python [--label 2.5.0] [--out results.json]

Starts `scenario_app.py` twice under the given interpreter (whatever WebCortex
it has installed): server F with WEBCORTEX_FAKE_PROVIDER=1, server R with no
fake and no provider keys. Then breaks things on purpose — 16 HTTP/MCP
scenarios and 2 CLI ones — and scores each failure's output on four criteria:

  a. code     the output names a stable error code (matches WC\\d{3})
  b. hint     the output carries an explicit hint: a non-empty "hint" field in
              the JSON (error.hint, the run result's hint, MCP
              structuredContent.error.hint or JSON-RPC error.data.hint), or a
              CLI line starting "hint:"
  c. json     the failure is machine-readable: the output parses as JSON
  d. status   the failure is visible in the live status view: the request id
              we sent (or the run id) appears in GET /_webcortex/status.
              Not applicable to CLI scenarios (no server).

Also reported, as a looser and secondary signal: whether the message text
suggests a fix (a keyword heuristic, see FIX_WORDS) — so a version whose
messages carry remedies in prose gets that credit.

Stdlib only. No real model API is called: providers are the offline fake, a
missing key, or a closed local port.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ADMIN = "bench-admin-key-0123456789abcdef0123"
USER = "bench-user-key-0123456789abcdef01234"
CODE = re.compile(r"\bWC\d{3}\b")
FIX_WORDS = ("set ", "export ", "declare", "use ", "add ", "release", "list the origin", "try:",
             "requires", "send ", "raise ")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start(python: str, fake: bool, workdir: Path) -> tuple[subprocess.Popen, str]:
    port = free_port()
    env = {k: v for k, v in os.environ.items()
           if k not in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "WEBCORTEX_FAKE_PROVIDER")}
    env.update({"BENCH_PORT": str(port), "BENCH_ADMIN_KEY": ADMIN, "BENCH_USER_KEY": USER,
                "WEBCORTEX_LOG": "error"})
    if fake:
        env["WEBCORTEX_FAKE_PROVIDER"] = "1"
    log = open(workdir / f"server-{'fake' if fake else 'real'}.log", "w")
    proc = subprocess.Popen([python, "-m", "webcortex.cli", "run", str(HERE / "scenario_app.py")],
                            cwd=workdir, env=env, stdout=log, stderr=subprocess.STDOUT)
    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 60
    while time.time() < deadline:
        if proc.poll() is not None:
            raise SystemExit(f"server exited: {(workdir / log.name).read_text()}")
        try:
            urllib.request.urlopen(base + "/_webcortex/health", timeout=1)
            return proc, base
        except Exception:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit("server never became ready")


def http(base, path, method="GET", body=None, key=None, headers=None):
    data = json.dumps(body).encode() if body is not None else None
    h = {"content-type": "application/json"} if data is not None else {}
    h.update(headers or {})
    if key:
        h["x-api-key"] = key
    req = urllib.request.Request(base + path, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")


# (name, server, method, path, body, key, extra headers, what counts as "it failed")
def scenarios():
    mcp = lambda name: {"jsonrpc": "2.0", "id": 1, "method": "tools/call",  # noqa: E731
                        "params": {"name": name, "arguments": {}}}
    return [
        ("missing provider key", "R", "POST", "/agents/keyless", {"input": "hi"}, USER, {}, "status>=400"),
        ("provider unreachable", "R", "POST", "/agents/deadagent", {"input": "hi"}, USER, {}, "status>=400"),
        ("bad route", "F", "GET", "/no/such/route", None, USER, {}, "status>=400"),
        ("wrong method", "F", "DELETE", "/ping", None, USER, {}, "status>=400"),
        ("budget exceeded", "F", "POST", "/agents/tight", {"input": "tool:ping {}"}, USER, {}, "run"),
        ("actuator called while halted", "F", "POST", "/arm/move", {"x": 1}, ADMIN, {}, "status>=400"),
        ("malformed agent request", "F", "POST", "/agents/desk", {"question": "hi"}, USER, {}, "status>=400"),
        ("tool argument missing (HTTP)", "F", "POST", "/notes", {}, USER, {}, "status>=400"),
        ("tool argument missing (inside an agent run)", "F", "POST", "/agents/desk",
         {"input": "tool:add_note {}"}, USER, {}, "step"),
        ("unauthenticated admin call", "F", "GET", "/_webcortex/usage", None, None, {}, "status>=400"),
        ("authenticated but missing scope", "F", "POST", "/arm/move", {"x": 1}, USER, {}, "status>=400"),
        ("cross-origin browser POST", "F", "POST", "/notes", {"title": "x"}, USER,
         {"origin": "https://evil.example", "sec-fetch-site": "cross-site"}, "status>=400"),
        ("unknown device", "F", "GET", "/devices/ghost/latest", None, USER, {}, "status>=400"),
        ("camera has sent no frame", "F", "GET", "/devices/cam/latest", None, USER, {}, "status>=400"),
        ("Python handler raised", "F", "GET", "/boom", None, USER, {}, "status>=400"),
        ("gated tool called over MCP", "F", "POST", "/_webcortex/mcp", mcp("move"), ADMIN, {}, "mcp"),
    ]


def find_hint(value) -> bool:
    if isinstance(value, dict):
        for k, v in value.items():
            if k == "hint" and isinstance(v, str) and v.strip():
                return True
            if find_hint(v):
                return True
    elif isinstance(value, list):
        return any(find_hint(v) for v in value)
    return False


def failed(kind: str, status: int, parsed) -> bool:
    if kind == "status>=400":
        return status >= 400
    if kind == "run":
        return isinstance(parsed, dict) and parsed.get("status") not in (None, "completed")
    if kind == "step":
        return isinstance(parsed, dict) and any(s.get("error") for s in parsed.get("steps", []))
    if kind == "mcp":
        return isinstance(parsed, dict) and (parsed.get("result", {}).get("isError") or "error" in parsed)
    return False


def score_http(base_for, python) -> list[dict]:
    rows = []
    for name, server, method, path, body, key, headers, kind in scenarios():
        base = base_for[server]
        rid = f"bench-{len(rows)}-{time.time_ns()}"
        status, text = http(base, path, method, body, key, {**headers, "x-request-id": rid})
        try:
            parsed = json.loads(text)
        except ValueError:
            parsed = None
        did_fail = failed(kind, status, parsed)
        run_id = parsed.get("run_id") if isinstance(parsed, dict) else None
        st_status, st_text = http(base, "/_webcortex/status", key=ADMIN)
        in_status = st_status == 200 and (rid in st_text or bool(run_id and run_id in st_text))
        rows.append({
            "scenario": name,
            "failed": did_fail,
            "http_status": status,
            "code": did_fail and bool(CODE.search(text)),
            "hint": did_fail and find_hint(parsed),
            "json": did_fail and parsed is not None,
            "status_view": did_fail and in_status,
            "fix_in_message": did_fail and any(w in text.lower() for w in FIX_WORDS),
            "output": text[:600],
        })
    return rows


def score_cli(python: str, workdir: Path) -> list[dict]:
    rows = []
    env = {k: v for k, v in os.environ.items() if k != "WEBCORTEX_FAKE_PROVIDER"}
    for name, target in (("bad manifest (agent names a missing tool)", "broken_app.py"),
                         ("app module fails to import", "import_error_app.py")):
        p = subprocess.run([python, "-m", "webcortex.cli", "check", str(HERE / target)],
                           cwd=workdir, env=env, capture_output=True, text=True, timeout=60)
        out = p.stdout + p.stderr
        try:
            parsed = json.loads(p.stdout)
        except ValueError:
            parsed = None
        did_fail = p.returncode != 0
        rows.append({
            "scenario": name,
            "failed": did_fail,
            "exit_code": p.returncode,
            "code": did_fail and bool(CODE.search(out)),
            "hint": did_fail and (find_hint(parsed) or any(l.strip().startswith("hint:") for l in out.splitlines())),
            "json": did_fail and parsed is not None,
            "status_view": None,
            "fix_in_message": did_fail and any(w in out.lower() for w in FIX_WORDS),
            "output": out[-600:],
        })
    return rows


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--python", default=sys.executable)
    ap.add_argument("--label", default=None)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    version = subprocess.run([args.python, "-c", "import webcortex; print(webcortex.__version__)"],
                             capture_output=True, text=True, check=True).stdout.strip()
    workdir = Path(tempfile.mkdtemp(prefix="wcx-diagbench-"))
    procs = []
    try:
        f_proc, f_base = start(args.python, True, workdir)
        procs.append(f_proc)
        r_proc, r_base = start(args.python, False, workdir)
        procs.append(r_proc)
        rows = score_http({"F": f_base, "R": r_base}, args.python) + score_cli(args.python, workdir)
    finally:
        for p in procs:
            p.terminate()
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()

    applicable = lambda k: [r for r in rows if r[k] is not None]  # noqa: E731
    summary = {k: f"{sum(1 for r in applicable(k) if r[k])}/{len(applicable(k))}"
               for k in ("failed", "code", "hint", "json", "status_view", "fix_in_message")}
    repo = str(HERE.parent.parent)
    for r in rows:
        r["output"] = r["output"].replace(repo, "<repo>")
    result = {"version": args.label or version, "summary": summary, "scenarios": rows}
    text = json.dumps(result, indent=2)
    if args.out:
        Path(args.out).write_text(text + "\n")
    print(json.dumps({"version": result["version"], "summary": summary}, indent=2))
    for r in rows:
        marks = "".join("x" if r[k] else ("-" if r[k] is None else ".")
                        for k in ("code", "hint", "json", "status_view"))
        print(f"  {marks}  {'' if r['failed'] else '(did not fail) '}{r['scenario']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
