"""`webcortex doctor` and `webcortex status`: is it set up right, and what is it doing.

``doctor(app)`` runs checks that need no server — versions, the manifest,
provider keys (presence only, never values), security posture, the e-stop,
devices, budgets, the port — plus, with ``online=True``, a short reachability
probe of each model provider. Every check is ``{id, status, message, hint}``
with ``status`` one of ``ok | warn | fail``, and a ``code`` from the error
catalogue when it is not ok.

``fetch_status(...)`` reads ``GET {control_prefix}/status`` from a running app.
"""

from __future__ import annotations

import json
import os
import socket
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

__all__ = ["doctor", "fetch_status", "app_url", "render_doctor", "render_status"]


def _check(id: str, status: str, message: str, hint: str = "", code: str | None = None) -> dict:
    out = {"id": id, "status": status, "message": message, "hint": hint}
    if code:
        out["code"] = code
        if not hint:
            from .errors import hint as catalogue_hint

            out["hint"] = catalogue_hint(code)
    return out


def app_url(app: Any, url: str | None = None) -> str:
    """The base URL of the running app: ``--url``, else WEBCORTEX_HOST/PORT, else the app's own."""
    if url:
        return url.rstrip("/")
    host = os.environ.get("WEBCORTEX_HOST", getattr(app, "host", "127.0.0.1"))
    port = int(os.environ.get("WEBCORTEX_PORT", getattr(app, "port", 8000)))
    if host in ("0.0.0.0", "::", ""):
        host = "127.0.0.1"
    return f"http://{host}:{port}"


def _get_json(url: str, key: str | None, timeout: float) -> tuple[int, Any]:
    req = urllib.request.Request(url, headers={"accept": "application/json"})
    if key:
        req.add_header("x-api-key", key)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            return res.status, json.loads(res.read() or b"null")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"null")
        except ValueError:
            return e.code, None


def _models_used(app: Any) -> dict[str, list[str]]:
    """Every model name the app will ask for, and what asks for it."""
    used: dict[str, list[str]] = {}

    def add(model: str | None, who: str) -> None:
        if model:
            used.setdefault(model, []).append(who)

    for a in app._agents:
        add(a.get("model"), f"agent {a['name']}")
        policy = a.get("policy") or {}
        if policy.get("max_context_tokens"):
            add(policy.get("compact_with") or "fast", f"agent {a['name']} (compaction)")
    for b in app._behaviours:
        add(b.get("model"), f"behaviour {b['name']}")
    for f in app._flows:
        kind = f.get("kind") or {}
        if kind.get("kind") == "route":
            add(kind.get("classify_with") or "fast", f"flow {f['name']} (classifier)")
    return used


def _probe(base_url: str, timeout: float) -> tuple[bool, str]:
    """Any HTTP answer at all means reachable; only a transport failure is not."""
    req = urllib.request.Request(base_url.rstrip("/") + "/", method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            return True, f"answered {res.status}"
    except urllib.error.HTTPError as e:
        return True, f"answered {e.code}"
    except (urllib.error.URLError, OSError, ValueError) as e:
        reason = getattr(e, "reason", e)
        return False, f"{type(reason).__name__}: {reason}"


def doctor(app: Any, *, online: bool = False, url: str | None = None, key: str | None = None,
           timeout: float = 3.0) -> dict:
    """Check that `app` is ready to run. Never prints a secret's value.

    Returns ``{"ok", "summary": {"ok", "warn", "fail"}, "checks": [...]}``;
    ``ok`` is false when any check failed.
    """
    from . import __version__

    checks: list[dict] = []
    add = checks.append

    # -- the interpreter and the compiled core --------------------------------
    from ._bridge import free_threaded

    py = sys.version.split()[0]
    gil = "free-threaded" if free_threaded() else "GIL-enabled"
    if sys.version_info >= (3, 12):
        add(_check("python", "ok", f"Python {py} ({gil})"))
    else:
        add(_check("python", "fail", f"Python {py} is older than 3.12", "Use Python 3.12 or newer.",
                   code="WC103"))

    core_ok = False
    try:
        from . import _core

        core = getattr(_core, "__version__", "?")
        if core == __version__:
            core_ok = True
            add(_check("core", "ok", f"webcortex {__version__}, core extension {core}"))
        else:
            add(_check("core", "fail", f"webcortex {__version__} but the core extension is {core}",
                       code="WC103"))
    except Exception as e:  # noqa: BLE001 - any import failure is the finding
        add(_check("core", "fail", f"the core extension does not import: {type(e).__name__}: {e}",
                   code="WC103"))
    if not core_ok:
        return _summarise(checks)

    # -- the app ---------------------------------------------------------------
    add(_check("app", "ok", f"app {app.name!r} {app.version} loaded"))
    try:
        report = app.check()
        add(_check("manifest", "ok",
                   f"{report['routes']} routes ({report['native_routes']} native), "
                   f"{len(report['tools'])} tools, {len(report['agents'])} agents, "
                   f"{len(report['behaviours'])} behaviours, {len(report['flows'])} flows"))
    except Exception as e:  # noqa: BLE001
        add(_check("manifest", "fail", str(e), code="WC101"))
        return _summarise(checks)

    manifest = app.manifest()

    # -- model providers -------------------------------------------------------
    from . import _core

    used = _models_used(app)
    if used:
        diag = json.loads(_core.diagnose_models(app.manifest_json(), list(used)))
        if diag.get("fake"):
            add(_check("providers.fake", "warn",
                       "WEBCORTEX_FAKE_PROVIDER is set: every model call is answered by the offline fake",
                       "Unset WEBCORTEX_FAKE_PROVIDER outside tests."))
        endpoints: dict[str, list[str]] = {}
        for d in diag["models"]:
            who = ", ".join(used[d["model"]])
            label = d["model"] if d["model"] == d["resolved"] else f"{d['model']} → {d['resolved']}"
            if d["ok"]:
                add(_check(f"model:{d['model']}", "ok", f"{label} via {d['provider']} (used by {who})"))
                if d.get("base_url"):
                    endpoints.setdefault(d["base_url"], []).append(d["model"])
            else:
                add(_check(f"model:{d['model']}", "fail", f"{label} (used by {who}): {d['error']}",
                           code=d.get("code") or "WC300"))
        unpriced = [d["model"] for d in diag["models"] if d["ok"] and not d.get("priced")
                    and d.get("provider") not in ("fake", "ollama")]
        if unpriced:
            add(_check("pricing", "ok",
                       f"no app.pricing for {', '.join(unpriced)}: spend is counted, cost is reported as null"))
        if online:
            for base, models in endpoints.items():
                ok, detail = _probe(base, timeout)
                add(_check(f"online:{base}", "ok" if ok else "fail",
                           f"{base} {detail} (for {', '.join(models)})", code=None if ok else "WC303"))
        elif endpoints:
            add(_check("online", "ok", f"{len(endpoints)} provider endpoint(s) not probed; "
                                       "run `webcortex doctor --online` to check reachability"))
    else:
        add(_check("providers", "ok", "no agents, behaviours or flows: no model provider needed"))

    for name, prov in (manifest.get("models") or {}).get("providers", {}).items():
        env = prov.get("api_key_env")
        if env and not os.environ.get(env):
            add(_check(f"provider:{name}", "warn", f"provider {name!r} names {env}, which is not set",
                       code="WC106"))

    # -- credentials named by the app (presence only) ---------------------------
    for env, spec in manifest["auth"]["api_keys"].items():
        if os.environ.get(env):
            add(_check(f"api_key:{env}", "ok", f"{env} is set (principal {spec['id']!r})"))
        else:
            add(_check(f"api_key:{env}", "fail",
                       f"{env} is not set: principal {spec['id']!r} cannot authenticate",
                       f"export {env}=$(webcortex keygen)", code="WC106"))
    jwt = manifest["auth"].get("jwt")
    if jwt and not os.environ.get(jwt.get("secret_env", "")):
        add(_check("jwt", "fail", f"{jwt.get('secret_env')} (the JWT secret) is not set", code="WC106"))
    for name, up in (manifest.get("upstreams") or {}).items():
        env = up.get("bearer_env")
        if env and not os.environ.get(env):
            add(_check(f"upstream:{name}", "warn",
                       f"upstream {name!r} names {env}, which is not set: it will be called unauthenticated",
                       code="WC106"))

    # -- security posture --------------------------------------------------------
    sec = app.security_report()
    if not sec["auth_configured"]:
        add(_check("security.auth", "warn", "no authentication is configured: every route is public",
                   code="WC608"))
    else:
        add(_check("security.auth", "ok", f"auth configured; {len(sec['public_routes'])} public route(s)"))
    if "*" in (sec.get("cors_origins") or []):
        add(_check("security.cors", "warn", "app.cors allows every origin ('*')",
                   "List the origins that need access instead of '*'.", code=None))
    if not sec["rate_limited"]:
        add(_check("security.rate_limit", "warn", "no rate limit: one caller can use every worker",
                   "app.rate_limit(per_second=50, burst=100)"))
    for w in sec.get("actuator_warnings", []):
        add(_check("security.actuators", "warn", w,
                   "Scope actuators, gate agent-callable ones with approval='required', and configure auth."))

    # -- e-stop ------------------------------------------------------------------
    if sec.get("actuators"):
        names = ", ".join(a["name"] for a in sec["actuators"])
        if sec.get("start_halted"):
            add(_check("estop", "ok", f"actuators ({names}) boot halted; `webcortex release` arms them"))
        else:
            add(_check("estop", "warn", f"actuators ({names}) are armed at boot",
                       "WebCortex(..., start_halted=True) makes a restart wait for an operator."))

    # -- devices and watchers -----------------------------------------------------
    agents = {a["name"]: a for a in app._agents}
    for d in app._devices:
        src = d.get("source")
        if src and src.get("bearer_env") and not os.environ.get(src["bearer_env"]):
            add(_check(f"device:{d['name']}", "warn",
                       f"camera {d['name']!r} names {src['bearer_env']}, which is not set", code="WC106"))
    for w in app._watchers:
        a = agents.get(w["agent"])
        problems = []
        if a is not None and not a.get("token_budget"):
            problems.append("its agent has no token_budget")
        hook = (w.get("webhook") or {}).get("url") or ""
        if hook.startswith("http://") and not hook.startswith(("http://127.", "http://localhost")):
            problems.append("its webhook is plain http")
        if problems:
            add(_check(f"watcher:{w['name']}", "warn",
                       f"watcher {w['name']!r} runs {w['agent']} every {w['every_secs']:g}s, up to "
                       f"{w['max_runs_per_hour']}/h, and {'; '.join(problems)}",
                       "Give watcher agents a token_budget; use https webhooks."))
        else:
            add(_check(f"watcher:{w['name']}", "ok",
                       f"watcher {w['name']!r}: {w['device']} → {w['agent']} every {w['every_secs']:g}s"))

    # -- budgets ------------------------------------------------------------------
    unbounded = [f"agent {a['name']}" for a in app._agents if not a.get("token_budget")]
    unbounded += [f"behaviour {b['name']}" for b in app._behaviours if not b.get("token_budget")]
    if unbounded:
        add(_check("budgets", "warn", f"no token_budget on {', '.join(unbounded)}: only max_steps bounds a run",
                   "token_budget=... caps a run's spend (and the whole request tree it starts).", code="WC402"))
    elif app._agents or app._behaviours:
        add(_check("budgets", "ok", "every agent and behaviour has a token_budget"))

    # -- storage -------------------------------------------------------------------
    db = (manifest.get("database") or {}).get("url") or ""
    if db.startswith("sqlite://") and ":memory:" not in db:
        parent = Path(db[len("sqlite://"):].split("?")[0]).expanduser().resolve().parent
        if parent.is_dir():
            add(_check("database", "ok", f"{db}"))
        else:
            add(_check("database", "fail", f"{parent} does not exist (database {db})", code="WC107"))
    if app.templates_dir and not Path(app.templates_dir).is_dir():
        add(_check("templates", "fail", f"templates directory {app.templates_dir} does not exist", code="WC107"))

    # -- the port / a running instance ----------------------------------------------
    base = app_url(app, url)
    host, port = manifest["server"]["host"], int(manifest["server"]["port"])
    probe_host = "127.0.0.1" if host in ("0.0.0.0", "::", "") else host
    in_use = False
    try:
        with socket.create_connection((probe_host, port), timeout=0.5):
            in_use = True
    except OSError:
        pass
    if url or in_use:
        key = key or os.environ.get("WEBCORTEX_API_KEY")
        try:
            code, health = _get_json(f"{base}{app.control_prefix}/health", key, timeout)
        except (urllib.error.URLError, OSError, ValueError):
            code, health = 0, None
        if isinstance(health, dict) and health.get("app") == app.name:
            add(_check("running", "ok", f"{app.name} is running at {base} (version {health.get('version')})"))
            halted = health.get("halted")
            if halted is not None and sec.get("actuators"):
                add(_check("estop.live", "warn" if not halted else "ok",
                           "the running app's actuators are " + ("HALTED" if halted else "armed")))
        elif in_use and not url:
            add(_check("port", "fail", f"{probe_host}:{port} is in use by something that is not {app.name}",
                       code="WC104"))
        else:
            add(_check("running", "warn", f"nothing answered at {base}", code="WC108"))
    else:
        add(_check("port", "ok", f"{probe_host}:{port} is free"))

    return _summarise(checks)


def _summarise(checks: list[dict]) -> dict:
    summary = {s: sum(1 for c in checks if c["status"] == s) for s in ("ok", "warn", "fail")}
    return {"ok": summary["fail"] == 0, "summary": summary, "checks": checks}


_MARK = {"ok": "ok  ", "warn": "WARN", "fail": "FAIL"}


def render_doctor(report: dict) -> str:
    lines = []
    for c in report["checks"]:
        code = f" [{c['code']}]" if c.get("code") and c["status"] != "ok" else ""
        lines.append(f"  {_MARK[c['status']]}  {c['id']}{code}: {c['message']}")
        if c["status"] != "ok" and c.get("hint"):
            lines.append(f"          hint: {c['hint']}")
    s = report["summary"]
    lines.append("")
    lines.append(f"  {s['ok']} ok, {s['warn']} warning(s), {s['fail']} failure(s)")
    return "\n".join(lines)


def fetch_status(app: Any = None, *, url: str | None = None, key: str | None = None,
                 timeout: float = 5.0) -> dict:
    """GET the running app's live status. Raises ``RuntimeError`` with a code on failure."""
    prefix = getattr(app, "control_prefix", "/_webcortex") if app is not None else "/_webcortex"
    base = app_url(app, url) if app is not None else (url or "http://127.0.0.1:8000").rstrip("/")
    key = key or os.environ.get("WEBCORTEX_API_KEY")
    target = f"{base}{prefix}/status"
    try:
        status, body = _get_json(target, key, timeout)
    except (urllib.error.URLError, OSError) as e:
        raise StatusError("WC108", f"cannot reach {target}: {getattr(e, 'reason', e)}") from None
    if status != 200 or not isinstance(body, dict):
        err = (body or {}).get("error", {}) if isinstance(body, dict) else {}
        code = err.get("code") or ("WC212" if status == 404 else "WC001")
        raise StatusError(code, f"{target} answered {status}: {err.get('message', body)}", err.get("hint"))
    return body


class StatusError(RuntimeError):
    def __init__(self, code: str, message: str, hint: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.hint = hint


def render_status(s: dict) -> str:
    out = []
    up = s.get("uptime_secs", 0)
    out.append(f"  {s['app']} {s.get('app_version', '')} · webcortex {s.get('webcortex')} · "
               f"up {up // 3600}h{(up % 3600) // 60:02d}m{up % 60:02d}s")
    estop = s.get("estop", {})
    if estop.get("actuators"):
        out.append(f"  e-stop: {'HALTED' if estop.get('halted') else 'armed'} "
                   f"({', '.join(estop['actuators'])})")
    total = s["requests"]["total"]
    if total.get("requests"):
        out.append(f"  requests: {total['requests']}  2xx {total['status']['2xx']}  "
                   f"4xx {total['status']['4xx']}  5xx {total['status']['5xx']}  "
                   f"p50 {total['p50_ms']}ms  p95 {total['p95_ms']}ms  max {total['max_ms']}ms")
        for r in s["requests"]["routes"][:8]:
            out.append(f"    {r['requests']:>7}  {r['route']}  (4xx {r['status']['4xx']}, "
                       f"5xx {r['status']['5xx']}, p95 {r['p95_ms']}ms)")
    else:
        out.append("  requests: none yet")
    errs = s["errors"]
    if errs["recent"]:
        out.append(f"  errors: {errs['total']} total; most recent:")
        for e in errs["recent"][:8]:
            st = e.get("status") or e.get("source")
            out.append(f"    {e['code']}  {st}  {e['route']}  {e.get('request_id', '')}")
    agents = s.get("agents", {})
    if agents.get("in_flight"):
        out.append(f"  runs in flight: {len(agents['in_flight'])}")
        for r in agents["in_flight"][:5]:
            out.append(f"    {r['agent']}  step {r['steps']}  {r['tokens']} tokens  {r['running_ms']}ms")
    if agents.get("recent"):
        out.append("  recent runs:")
        for r in agents["recent"][:8]:
            cost = "" if r.get("cost_usd") is None else f"  ${r['cost_usd']:.4f}"
            code = f"  {r['code']}" if r.get("code") else ""
            out.append(f"    {r['outcome']:<17} {r['agent']}  {r['steps']} steps  {r['tokens']} tokens{cost}{code}")
    usage = s.get("usage", {})
    if usage.get("total_tokens"):
        cost = usage.get("estimated_cost_usd")
        out.append(f"  spend: {usage['total_tokens']} tokens, "
                   + ("cost unknown (unpriced models)" if cost is None else f"${cost:.4f}"))
    devices = s.get("devices", {})
    for d in devices.get("devices", []):
        out.append(f"  device {d['name']}: {'connected' if d['connected'] else 'idle'}, {d['fps']} fps, "
                   f"{d['frames_in']} in, {d['frames_dropped']} dropped, {d['subscribers']} subscriber(s)")
    for w in devices.get("watchers", []):
        st = w["stats"]
        out.append(f"  watcher {w['name']}: {st.get('runs', 0)} runs, {st.get('failed', 0)} failed"
                   + (f", last {st['last_status']}" if st.get("last_status") else ""))
    return "\n".join(out)
