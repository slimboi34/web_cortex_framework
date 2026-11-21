"""The `webcortex` command line.

Convention over configuration: every command defaults to `api.py` in the current
directory and expects it to define `app`.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import shlex
import sys
from pathlib import Path
from typing import Any

from .starters import TEMPLATES

DEFAULT_MODULE = "api.py"


class CLIError(Exception):
    """A failure the CLI reports with a catalogued code (see `webcortex explain`)."""

    def __init__(self, code: str, message: str, hint: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.hint = hint

    def as_json(self) -> dict:
        from .errors import hint as catalogue_hint

        return {"ok": False, "error": {"code": self.code, "message": self.message,
                                       "hint": self.hint if self.hint is not None else catalogue_hint(self.code)}}


def _load_app(target: str) -> Any:
    """Import a module by path and return its `app`.

    Accepts `api.py`, `api`, or `pkg.mod:name`.
    """
    module_part, attr = _split_target(target)

    path = Path(module_part)
    if not path.suffix:
        path = path.with_suffix(".py")

    try:
        module = _import_target(path, module_part)
    except CLIError:
        raise
    except SystemExit:
        raise
    except Exception as e:  # noqa: BLE001 - whatever the module raised is the finding
        import traceback

        frames = traceback.extract_tb(e.__traceback__)
        package = str(Path(__file__).resolve().parent)
        inside = [str(Path(f.filename).resolve()).startswith(package) for f in frames]
        user = [f for f, mine in zip(frames, inside) if not mine]
        where = f" (at {user[-1].filename}:{user[-1].lineno})" if user else ""
        if isinstance(e, SyntaxError) and e.filename:
            # The traceback of a SyntaxError ends inside importlib; the error
            # itself knows the file and line that failed to parse.
            where = f" (at {e.filename}:{e.lineno})"
        detail = f"{type(e).__name__}: {e}{where}"
        # A ValueError raised by webcortex itself is a declaration it refused.
        declaration = isinstance(e, ValueError) and bool(inside) and inside[-1]
        code = "WC101" if declaration else "WC102"
        hint = ("A declaration in the app is invalid; the message names it and the location is the "
                "line that declared it.") if declaration else (
                f"Importing {path} raised. Run `python {path}` to see the full traceback.")
        raise CLIError(code, f"loading {target} failed: {detail}", hint) from None

    if not hasattr(module, attr):
        raise CLIError(
            "WC102",
            f"{module_part} does not define {attr!r}",
            f"Define `{attr} = WebCortex(...)` or pass module:name.",
        )
    return getattr(module, attr)


def _split_target(target: str) -> tuple[str, str]:
    """``module[:attr]`` → ``(module, attr)``.

    Split at the *last* colon, and only when what follows is a Python name:
    ``C:\\app\\api.py`` is a Windows path, not module ``C`` attribute
    ``\\app\\api.py``.
    """
    module_part, sep, attr = target.rpartition(":")
    if sep and attr.isidentifier() and module_part:
        return module_part, attr
    return target, "app"


def _import_target(path: Path, module_part: str) -> Any:
    if path.exists():
        # Sibling modules must be importable: an app split across api.py and
        # agents.py is the expected shape, not an edge case.
        directory = str(path.parent.resolve())
        if directory not in sys.path:
            sys.path.insert(0, directory)
        spec = importlib.util.spec_from_file_location(path.stem, path)
        if spec is None or spec.loader is None:
            raise CLIError("WC102", f"cannot load {path}")
        module = importlib.util.module_from_spec(spec)
        sys.modules[path.stem] = module
        spec.loader.exec_module(module)
    else:
        if not path.exists() and module_part.endswith(".py"):
            raise CLIError("WC102", f"{path} does not exist",
                           "Run the command where api.py lives, or pass the module path.")
        # `webcortex run pkg.api:app` names a module relative to where the
        # command runs. The console script puts its own bin directory on
        # sys.path, not the working directory, so add it.
        cwd = os.getcwd()
        if cwd not in sys.path:
            sys.path.insert(0, cwd)
        module = importlib.import_module(module_part)
    return module


def _load_dotenv() -> None:
    """Load `.env` if present. Existing environment always wins.

    The common dotenv syntax: `KEY=value`, `export KEY=value`, values in
    single or double quotes (kept verbatim, `#` included), and a trailing
    ` # comment` after an unquoted value.
    """
    path = Path(".env")
    if not path.is_file():
        return
    try:
        text = path.read_text(encoding="utf-8-sig")
    except (OSError, UnicodeDecodeError):
        return
    for line in text.splitlines():
        parsed = _dotenv_line(line)
        if parsed:
            os.environ.setdefault(*parsed)


def _dotenv_line(line: str) -> tuple[str, str] | None:
    line = line.strip()
    if not line or line.startswith("#") or "=" not in line:
        return None
    key, _, value = line.partition("=")
    key = key.strip()
    if key.startswith("export "):
        key = key[len("export "):].strip()
    if not key or not key.replace("_", "a").replace(".", "a").isalnum():
        return None
    value = value.strip()
    if len(value) >= 2 and value[0] in "'\"" and value[0] in value[1:]:
        value = value[1:value.index(value[0], 1)]
    else:
        # An unquoted value ends at a ` #` comment.
        for marker in (" #", "\t#"):
            if marker in value:
                value = value[:value.index(marker)].rstrip()
    return key, value


def _banner(app: Any) -> None:
    from ._bridge import free_threaded

    report = app.check()
    security = app.security_report()
    gil = "free-threaded" if free_threaded() else "GIL-enabled"

    from . import __version__

    print(f"  webcortex {__version__}  ·  {app.name} {app.version}")
    print(f"  python {sys.version.split()[0]} ({gil})")
    print(f"  {report['routes']} routes, {report['native_routes']} served without touching Python")

    if report["tools"]:
        shown = ", ".join(report["tools"][:5])
        more = f" … +{len(report['tools']) - 5}" if len(report["tools"]) > 5 else ""
        print(f"  {len(report['tools'])} agent tools: {shown}{more}")
    if report["agents"]:
        print(f"  agents: {', '.join(report['agents'])}")
    behaviours = security.get("behaviours", [])
    if behaviours:
        print(f"  behaviours: {', '.join(b['name'] for b in behaviours)}")
    flows = security.get("flows", [])
    if flows:
        print(f"  flows: {', '.join(f'{f['name']} ({f['kind']})' for f in flows)}")
    if security.get("memories"):
        print(f"  memory: {', '.join(security['memories'])}")
    manifest = app.manifest()
    aliases = manifest.get("models", {}).get("aliases", {})
    if aliases:
        print(f"  models: {', '.join(f'{k}={v}' for k, v in aliases.items())}")

    posture = []
    posture.append("auth" if security["auth_configured"] else "NO AUTH")
    if security["rate_limited"]:
        posture.append("rate-limited")
    if security["cors_enabled"]:
        posture.append("cors")
    if security["security_headers"]:
        posture.append("headers")
    print(f"  security: {', '.join(posture)}")

    # Surfaced on every boot, not buried in an audit command, because an
    # unintentionally public route is the failure that actually happens.
    if security["auth_configured"] and security["public_routes"]:
        print(f"  ⚠ {len(security['public_routes'])} route(s) need no credential "
              f"(run `webcortex security` to list them)")
    if security["gated_tools"]:
        print(f"  approval-gated tools: {', '.join(security['gated_tools'])}")
    if security.get("devices"):
        names = ", ".join(f"{d['name']} ({d['kind']}{', pulled' if d['pulled'] else ''})"
                          for d in security["devices"])
        print(f"  devices: {names}")
    for w in security.get("watchers", []):
        hook = f" → {w['webhook']}" if w["webhook"] else ""
        print(f"  watcher {w['name']}: {w['device']} → {w['agent']} every {w['every_secs']:g}s, "
              f"≤{w['max_runs_per_hour']} runs/h{hook}")
    if security.get("actuators"):
        names = ", ".join(a["name"] for a in security["actuators"])
        halted = "  (booting HALTED: `webcortex release` to enable)" if security.get("start_halted") else ""
        print(f"  actuators: {names}{halted}")
        print("  emergency stop: `webcortex halt`")
        for warning in security.get("actuator_warnings", []):
            print(f"  ⚠ {warning}")

    # The manifest applies WEBCORTEX_HOST and WEBCORTEX_PORT: print what binds.
    server = manifest["server"]
    prefix, host, port = server["control_prefix"], server["host"], server["port"]
    print(f"  http://{host}:{port}{prefix}/openapi.json   ·   MCP: http://{host}:{port}{prefix}/mcp")
    if report["agents"] or behaviours or flows:
        print(f"  usage: http://{host}:{port}{prefix}/usage   ·   approvals: http://{host}:{port}{prefix}/approvals")
    print()


EVOLVE_SYSTEM = """\
You extend applications built on WebCortex 2, a Python web framework with a Rust
core where every declared route is also an agent tool. You will be given a
context pack describing the current application and a cheat sheet of the
framework's API, then a request.

Respond with Python only: the code to add to api.py, complete and runnable,
with a one-line comment above each declaration saying what it is for. Prefer
declarations (app.resource, app.query, app.context, app.flow, app.memory) over
Python handlers; use a handler only for logic that cannot be declared. Declare
scopes on everything; gate destructive tools with approval="required". Use the
`fast` model alias for classification and extraction leaves. Do not repeat
declarations that already exist. Do not wrap the answer in prose.
"""


def _cmd_evolve(app: Any, args: argparse.Namespace) -> int:
    """Ask a model to propose an extension, anchored on the context pack."""
    from . import _core

    pack = app.context_pack()
    prompt = f"{pack}\n\n## Request\n\n{args.request}\n"
    if args.json:
        schema = json.dumps({
            "type": "object",
            "properties": {
                "summary": {"type": "string"},
                "code": {"type": "string"},
                "notes": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["summary", "code"],
        })
        out = _core.ask_model(app.manifest_json(), args.model, EVOLVE_SYSTEM, prompt, schema, 8192)
        text = json.dumps(json.loads(out), indent=2)
    else:
        text = _core.ask_model(app.manifest_json(), args.model, EVOLVE_SYSTEM, prompt, None, 8192)
        text = _strip_fence(text)

    if args.out:
        path = Path(args.out)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text.rstrip() + "\n")
        print(f"Wrote {path} ({len(text.splitlines())} lines) using model {args.model!r}. Review before merging into api.py.",
              file=sys.stderr)
    else:
        print(text.rstrip())
    return 0


def _strip_fence(text: str) -> str:
    t = text.strip()
    if t.startswith("```"):
        first_newline = t.find("\n")
        t = t[first_newline + 1:] if first_newline != -1 else t[3:]
        if t.rstrip().endswith("```"):
            t = t.rstrip()[:-3]
    return t


def mcp_server_name(name: str | None) -> str:
    """The app's name as an MCP server name: letters, digits, ``_`` and ``-``
    only, so it is one word in a shell command and a plain key in a config."""
    slug = re.sub(r"[^A-Za-z0-9_-]+", "-", name or "").strip("-")
    return slug or "webcortex"


def mcp_config(app: Any, key: str | None = None) -> dict:
    """The `mcpServers` entry an MCP client needs to reach this app.

    Streamable HTTP at the app's MCP endpoint; the API key travels as the same
    `x-api-key` header the REST side uses, so the client gets exactly the scopes
    that key has and nothing more. The server is named after the app, reduced
    to ``[A-Za-z0-9_-]`` (see :func:`mcp_server_name`).
    """
    from .app import resolve_port

    host = os.environ.get("WEBCORTEX_HOST", app.host)
    port = resolve_port(app.port)
    prefix = getattr(app, "control_prefix", "/_webcortex")
    url = f"http://{host}:{port}{prefix}/mcp"
    name = mcp_server_name(getattr(app, "name", None))
    return {"mcpServers": {name: {"type": "http", "url": url, "headers": {"x-api-key": key} if key else {}}}}


def claude_mcp_add_command(name: str, url: str, key: str | None) -> str:
    """The `claude mcp add` line for this app, quoted for a POSIX shell.

    Every argument is quoted with :func:`shlex.join`, so a name, URL or key
    holding a space, ``;``, ``$`` or a backtick is passed as written rather
    than run or expanded. Without a key the header is left as
    ``"x-api-key: $WEBCORTEX_API_KEY"`` in double quotes, for the shell to fill
    in from the environment when the line is pasted.
    """
    words = ["claude", "mcp", "add", "--transport", "http", name, url, "--header"]
    header = shlex.quote(f"x-api-key: {key}") if key else '"x-api-key: $WEBCORTEX_API_KEY"'
    return f"{shlex.join(words)} {header}"


def _control(app: Any, method: str, path: str, body: dict | None = None,
             key: str | None = None, url: str | None = None) -> dict:
    """Call the running app's control plane with the operator's API key.

    ``url`` is the running app's base URL (``--url``), which replaces the
    address derived from WEBCORTEX_HOST/PORT or the app; ``app`` may be None
    when it is given, and the control prefix is then the default.
    """
    import urllib.error
    import urllib.request

    from .diagnostics import app_url, credential_headers, operator_credential

    prefix = getattr(app, "control_prefix", "/_webcortex") if app is not None else "/_webcortex"
    base = app_url(app, url) if app is not None else (url or "http://127.0.0.1:8000").rstrip("/")
    target = f"{base}{prefix}{path}"
    data = json.dumps(body or {}).encode() if method == "POST" else None
    try:
        req = urllib.request.Request(target, data=data, method=method,
                                     headers={"content-type": "application/json"})
    except ValueError as e:
        raise CLIError("WC108", f"{target} is not a URL: {e}",
                       "Pass the running app's base URL, e.g. --url https://app.example.com") from None
    for name, value in credential_headers(operator_credential(key)).items():
        req.add_header(name, value)
    not_webcortex = ("Check --url (or WEBCORTEX_HOST/WEBCORTEX_PORT) points at the running WebCortex app, "
                     "without a path.")
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            raw = res.read()
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            err = json.loads(raw).get("error", {})
        except (ValueError, AttributeError):
            err = {}
        if not isinstance(err, dict) or not err:
            if e.code == 404:
                raise CLIError("WC108", f"{target} answered 404, and not with a WebCortex error",
                               not_webcortex) from None
            err = {}
        from .errors import code_for_status

        raise CLIError(err.get("code") or code_for_status(e.code), f"{target} answered {e.code}: "
                       f"{err.get('message', raw)}", err.get("hint")) from None
    except urllib.error.URLError as e:
        raise CLIError("WC108", f"cannot reach {target}: {e.reason}") from None
    except OSError as e:
        raise CLIError("WC108", f"cannot reach {target}: {e}") from None
    try:
        document = json.loads(raw or b"{}")
    except ValueError:
        document = None
    if not isinstance(document, dict):
        raise CLIError("WC108", f"{target} answered, but not with a WebCortex document", not_webcortex)
    return document


_REDACTED = "<redacted>"


def _redact_url(url: Any) -> Any:
    """A URL with any ``user:password@`` replaced."""
    if not isinstance(url, str):
        return url
    return re.sub(r"^([A-Za-z][A-Za-z0-9+.-]*://)[^/@]*@", rf"\1{_REDACTED}@", url)


def redacted_manifest(manifest: dict) -> dict:
    """The manifest with the credentials an app may have written into it hidden.

    `app.upstream(headers=...)` and `app.camera(headers=...)` hold header values
    as given, which for an API that authenticates with a header is the
    credential itself; a URL may carry ``user:password@``. Those values become
    ``"<redacted>"``. The names stay, so the shape of the app is still there to
    read, and environment variable *names* (``bearer_env``, ``api_key_env``)
    are not secrets.
    """
    m = json.loads(json.dumps(manifest))
    for up in (m.get("upstreams") or {}).values():
        up["headers"] = {k: _REDACTED for k in (up.get("headers") or {})}
        up["base_url"] = _redact_url(up.get("base_url"))
    for device in m.get("devices") or []:
        source = device.get("source")
        if isinstance(source, dict):
            source["headers"] = {k: _REDACTED for k in (source.get("headers") or {})}
            source["url"] = _redact_url(source.get("url"))
    for watcher in m.get("watchers") or []:
        hook = watcher.get("webhook")
        if isinstance(hook, dict):
            hook["url"] = _redact_url(hook.get("url"))
    database = m.get("database")
    if isinstance(database, dict):
        database["url"] = _redact_url(database.get("url"))
    return m


def route_table(app: Any) -> list[dict]:
    """Every route the app serves: method, path, the engine that executes it,
    the tool it is exposed as (or None) and the scopes it requires."""
    from .contextpack import _derive_tool_name

    rows = []
    for r in app.manifest()["routes"]:
        tool = r.get("tool") or {}
        name = (tool.get("name") or _derive_tool_name(r["method"], r["path"])) if tool.get("expose") else None
        rows.append({"method": r["method"], "path": r["path"], "engine": (r.get("op") or {}).get("kind"),
                     "tool": name, "scopes": list(r.get("scopes") or [])})
    return rows


def _boot_failure(app: Any, message: str, default: str = "WC101") -> CLIError:
    """A runtime refusal to build or serve the app, as a coded CLI error that
    names the file (and line) to fix instead of a Python traceback."""
    from .diagnostics import boot_error_code

    code = boot_error_code(message, default)
    hint = None
    if code == "WC104" and not any(t in message for t in ("in use", "os error 48", "os error 98",
                                                          "os error 10048")):
        hint = ("The address cannot be listened on. Choose another with --host/--port (or "
                "WEBCORTEX_HOST/WEBCORTEX_PORT); ports below 1024 need extra privileges.")
    if code == "WC004":
        message = _template_location(app, message)
    return CLIError(code, message, hint)


def _template_location(app: Any, message: str) -> str:
    """Point a template error at the file: ``(in p.html:3)`` becomes
    ``(at templates/p.html:3)``; a missing template names where it was looked for."""
    directory = getattr(app, "templates_dir", None)
    if not directory:
        return message
    at = re.search(r"\(in ([^():]+):(\d+)\)", message)
    if at:
        where = Path(directory) / at.group(1)
        return f"{message[:at.start()]}(at {where}:{at.group(2)}){message[at.end():]}"
    missing = re.search(r'template "([^"]+)" failed to load: .*(not found|does not exist)', message)
    if missing:
        return f"{message} (expected at {Path(directory) / missing.group(1)})"
    return message


def _cmd_new(args: argparse.Namespace) -> int:
    from . import starters

    name = args.name
    target = Path(args.directory or name)
    if target.exists() and (not target.is_dir() or any(target.iterdir())):
        raise SystemExit(f"webcortex: {target} already exists and is not an empty directory")

    try:
        files = starters.files_for(args.template, name, args.description or f"A WebCortex app: {name}")
    except ValueError as e:
        raise SystemExit(f"webcortex: {e}")

    for relative, contents in files.items():
        path = target / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)

    print(f"Created {target}/ from the '{args.template}' starter:\n")
    for relative in sorted(files):
        print(f"  {target}/{relative}")
    print(
        f"\nNext:\n"
        f"  cd {target}\n"
        f"  export WEBCORTEX_API_KEY=$(webcortex keygen)\n"
        f"  webcortex doctor    # names anything else this starter needs\n"
        f"  webcortex dev\n"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = _parser()
    args = parser.parse_args(argv)
    try:
        try:
            return _run(parser, args)
        except ValueError as e:
            # The core refusing the manifest, from any command that reads it.
            if str(e).startswith(("invalid application", "invalid manifest")):
                raise CLIError("WC101", str(e)) from None
            raise
    except CLIError as e:
        if getattr(args, "json", False) or args.command == "check":
            print(json.dumps(e.as_json(), indent=2))
        else:
            from .errors import format_error

            print(format_error(e.code, e.message, e.hint), file=sys.stderr)
        return 1


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="webcortex", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    new = sub.add_parser("new", help="scaffold a new project")
    new.add_argument("name")
    new.add_argument(
        "--template", "-t", default="api",
        choices=TEMPLATES,
        help=("api: JSON+MCP · fullstack: adds pages · agent: adds an approval gate · "
              "behaviour: adds programmable procedures · orchestration: handoffs, flows, "
              "memory, context and a local model · robotics: a camera agents can see "
              "through, gated actuators and an emergency stop · hub: cameras and sensors "
              "streaming to agents, insights to subscribers and webhooks"),
    )
    new.add_argument("--directory", "-d", default=None)
    new.add_argument("--description", default=None)

    sub.add_parser("keygen", help="mint an API key")

    for name, help_text in [
        ("dev", "run the development server"),
        ("run", "run the server"),
        ("check", "validate the application and print its route table"),
        ("openapi", "print the OpenAPI document"),
        ("tools", "print the agent tool manifest"),
        ("sql", "print the DDL for declared resources"),
        ("security", "report the public attack surface"),
        ("typegen", "generate a typed TypeScript client"),
        ("context", "print the context pack: the app described for an AI coding tool"),
        ("mcp-config", "print the MCP client configuration for this app (Claude Code, Claude Desktop, Cursor)"),
        ("evolve", "ask a model to propose an extension, anchored on the context pack"),
        ("halt", "engage the emergency stop on the running app: every actuator refuses to run"),
        ("release", "release the emergency stop on the running app"),
        ("doctor", "check the app is ready to run: versions, manifest, provider keys, security, e-stop, port"),
        ("status", "what the running app is doing: requests, recent errors, agent runs, spend, devices"),
        ("eval", "run the app's declared evals in-process, replaying recorded model responses when present"),
    ]:
        p = sub.add_parser(name, help=help_text)
        if name == "evolve":
            p.add_argument("request", help="what to add, in plain language")
        p.add_argument("target", nargs="?", default=DEFAULT_MODULE,
                       help="module path or module:attr (default: api.py)")
        if name in ("dev", "run"):
            p.add_argument("--host", default=None)
            p.add_argument("--port", type=int, default=None)
            p.add_argument("--workers", type=int, default=None)
        if name == "typegen":
            p.add_argument("--out", "-o", default="client/api.ts")
        if name == "context":
            p.add_argument("--json", action="store_true",
                           help="emit the raw manifest instead (credential values redacted)")
            p.add_argument("--show-secrets", action="store_true",
                           help="with --json, print upstream and camera header values and URL passwords as "
                                "declared instead of <redacted>")
        if name in ("halt", "release"):
            p.add_argument("--key", default=None,
                           help="admin API key or JWT bearer token (default: $WEBCORTEX_API_KEY, then $WEBCORTEX_TOKEN)")
            p.add_argument("--url", default=None,
                           help="the running app's base URL (default: WEBCORTEX_HOST/PORT or the app's own)")
        if name == "halt":
            p.add_argument("--reason", "-r", default="", help="recorded in the audit log")
            p.add_argument("--status", action="store_true", help="only report whether it is halted")
        if name in ("doctor", "status"):
            p.add_argument("--json", action="store_true", help="machine-readable output")
            p.add_argument("--url", default=None,
                           help="the running app's base URL (default: WEBCORTEX_HOST/PORT or the app's own)")
            p.add_argument("--key", default=None, help="admin API key or JWT bearer token (default: $WEBCORTEX_API_KEY, then $WEBCORTEX_TOKEN)")
        if name == "eval":
            how = p.add_mutually_exclusive_group()
            how.add_argument("--record", action="store_true",
                             help="call the real providers and write each eval's cassette")
            how.add_argument("--replay", action="store_true",
                             help="replay cassettes only; an eval without one fails (use in CI)")
            how.add_argument("--live", action="store_true",
                             help="always call the providers, ignoring cassettes")
            p.add_argument("--dir", default="evals", help="where cassettes live (default: evals)")
            p.add_argument("--only", action="append", default=None, metavar="NAME",
                           help="run just this eval; may be repeated")
            p.add_argument("--list", action="store_true", help="list the declared evals and exit")
            p.add_argument("--json", action="store_true", help="machine-readable output")
        if name == "doctor":
            p.add_argument("--online", action="store_true",
                           help="also probe each model provider's endpoint (3 s timeout)")
        if name == "evolve":
            p.add_argument("--model", "-m", default="default",
                           help="model or alias, e.g. default, fast, ollama/qwen3.5:9b")
            p.add_argument("--out", "-o", default=None, help="write the proposal to a file")
            p.add_argument("--json", action="store_true",
                           help="ask for {summary, code, notes} as JSON")

    trace = sub.add_parser("trace", help="what a recent agent, flow or behaviour run did, as a span tree")
    trace.add_argument("trace_id", nargs="?", default=None,
                       help="a trace id (from a run result or the x-webcortex-trace header) or 'latest'; "
                            "omit to list the recent traces")
    trace.add_argument("--app", dest="target", default=DEFAULT_MODULE,
                       help="module path or module:attr (default: api.py)")
    trace.add_argument("--json", action="store_true", help="machine-readable output")
    trace.add_argument("--url", default=None,
                       help="the running app's base URL (default: WEBCORTEX_HOST/PORT or the app's own)")
    trace.add_argument("--key", default=None,
                       help="admin API key or JWT bearer token (default: $WEBCORTEX_API_KEY, then $WEBCORTEX_TOKEN)")

    explain = sub.add_parser("explain", help="explain an error code (WCnnn), or list them all")
    explain.add_argument("code", nargs="?", default=None)
    explain.add_argument("--list", action="store_true", help="list every code")
    explain.add_argument("--json", action="store_true", help="machine-readable output")
    return parser


def _cmd_explain(args: argparse.Namespace) -> int:
    from . import errors

    if args.list or not args.code:
        entries = list(errors.catalogue())
        if args.json:
            print(json.dumps({"errors": entries}, indent=2))
            return 0
        category = None
        for e in entries:
            if e["category"] != category:
                category = e["category"]
                print(f"\n{category}")
            status = f" ({e['status']})" if e["status"] else ""
            print(f"  {e['code']}  {e['title']}{status}")
        print("\n`webcortex explain CODE` for what to do about one.")
        return 0
    entry = errors.explain(args.code)
    if entry is None:
        raise CLIError("WC205", f"{args.code!r} is not a WebCortex error code",
                       "Codes look like WC201; `webcortex explain --list` shows them all.")
    if args.json:
        print(json.dumps(entry, indent=2))
        return 0
    status = f"  ·  HTTP {entry['status']}" if entry["status"] else ""
    print(f"{entry['code']}  {entry['title']}  ·  {entry['category']}{status}\n")
    print(entry["hint"])
    print(f"\n{entry['docs']}")
    return 0


def _cmd_doctor(args: argparse.Namespace) -> int:
    from .diagnostics import _check, _summarise, doctor, render_doctor

    try:
        app = _load_app(args.target)
    except CLIError as e:
        report = _summarise([_check("app", "fail", e.message, e.hint or "", code=e.code)])
    else:
        report = doctor(app, online=args.online, url=args.url, key=args.key)
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(render_doctor(report))
    return 0 if report["ok"] else 1


def _cmd_status(args: argparse.Namespace) -> int:
    from .diagnostics import StatusError, fetch_status, render_status

    app = None
    if not (args.url and args.target == DEFAULT_MODULE and not Path(DEFAULT_MODULE).exists()):
        app = _load_app(args.target)
    try:
        status = fetch_status(app, url=args.url, key=args.key)
    except StatusError as e:
        raise CLIError(e.code, e.message, e.hint) from None
    print(json.dumps(status, indent=2) if args.json else render_status(status))
    return 0


def _cmd_trace(args: argparse.Namespace) -> int:
    """`webcortex trace [ID]`: list the running app's recent traces, or print one."""
    from .diagnostics import StatusError, fetch_control, render_trace, render_traces

    app = None
    if not (args.url and args.target == DEFAULT_MODULE and not Path(DEFAULT_MODULE).exists()):
        app = _load_app(args.target)
    path = f"/traces/{args.trace_id}" if args.trace_id else "/traces"
    try:
        document = fetch_control(app, path, url=args.url, key=args.key)
    except StatusError as e:
        raise CLIError(e.code, e.message, e.hint) from None
    if args.json:
        print(json.dumps(document, indent=2))
    else:
        print(render_trace(document) if args.trace_id else render_traces(document))
    return 0


def _cmd_estop(args: argparse.Namespace) -> int:
    """`webcortex halt` / `webcortex release`: the emergency stop of a running app.

    With ``--url`` and no app module here, the operator needs nothing but the
    address and an admin credential, which is what an e-stop should need.
    """
    app = None
    if not (args.url and args.target == DEFAULT_MODULE and not Path(DEFAULT_MODULE).exists()):
        app = _load_app(args.target)
    if args.command == "release":
        state = _control(app, "POST", "/release", key=args.key, url=args.url)
        print("released: actuators enabled" if state.get("was_halted") else "was not halted")
        return 0
    if args.status:
        print(json.dumps(_control(app, "GET", "/halt", key=args.key, url=args.url), indent=2))
        return 0
    state = _control(app, "POST", "/halt", {"reason": args.reason}, key=args.key, url=args.url)
    print(f"HALTED: {(state.get('state') or {}).get('reason', '')}. Actuators refuse to run "
          f"until `webcortex release`.")
    return 0


def _cmd_eval(app: Any, args: argparse.Namespace) -> int:
    """`webcortex eval`: run the declared evals; exit 1 when any fails."""
    from .evals import _exists, cassette_path, render_report, run_evals

    if args.list:
        cases = [{"name": c.name, "target": c.target, "description": c.description,
                  "expect": c.expect, "cassette": _exists(cassette_path(args.dir, c.name))}
                 for c in app._evals]
        if args.json:
            print(json.dumps({"evals": cases}, indent=2,
                             default=lambda v: sorted(v) if isinstance(v, (set, frozenset)) else str(v)))
        else:
            for c in cases:
                print(f"  {c['name']:<32} → {c['target']}  "
                      f"{'cassette' if c['cassette'] else 'no cassette'}  {c['description']}")
            if not cases:
                print("  no evals declared")
        return 0
    mode = "record" if args.record else "replay" if args.replay else "live" if args.live else "auto"
    try:
        report = run_evals(app, mode=mode, directory=args.dir, only=args.only)
    except ValueError as e:
        if str(e).startswith(("invalid application", "invalid manifest")):
            raise
        raise CLIError("WC320", str(e), "`webcortex eval --list` shows the declared evals.") from None
    print(json.dumps(report, indent=2, default=str) if args.json else render_report(report))
    return 0 if report["ok"] else 1


def _run(parser: argparse.ArgumentParser, args: argparse.Namespace) -> int:
    if args.command == "new":
        return _cmd_new(args)

    if args.command == "explain":
        return _cmd_explain(args)

    if args.command == "keygen":
        from . import _core

        print(_core.generate_api_key())
        return 0

    _load_dotenv()
    if args.command == "doctor":
        return _cmd_doctor(args)
    if args.command == "status":
        return _cmd_status(args)
    if args.command == "trace":
        return _cmd_trace(args)
    if args.command in ("halt", "release"):
        return _cmd_estop(args)
    app = _load_app(args.target)

    if args.command == "eval":
        return _cmd_eval(app, args)

    if args.command in ("dev", "run"):
        # An explicit flag wins over WEBCORTEX_HOST/PORT, which the manifest
        # (and every later command) reads; set both so they agree.
        if args.host:
            app.host = args.host
            os.environ["WEBCORTEX_HOST"] = args.host
        if args.port:
            app.port = args.port
            os.environ["WEBCORTEX_PORT"] = str(args.port)
        if args.workers:
            app.workers = args.workers
        if args.command == "dev":
            os.environ.setdefault("WEBCORTEX_LOG", "info")
        try:
            _banner(app)
        except ValueError as e:
            raise _boot_failure(app, str(e)) from None
        try:
            app.run()
        except (RuntimeError, ValueError) as e:
            # Whatever the runtime refused at boot (a template that does not
            # parse, an address that cannot be bound, a database that will not
            # open) is a coded error naming what to fix, not a traceback.
            raise _boot_failure(app, str(e)) from None
        return 0

    if args.command == "check":
        try:
            report = app.check()
            table = route_table(app)
        except ValueError as e:
            raise _boot_failure(app, str(e)) from None
        print(json.dumps({**report, "route_table": table, "security": app.security_report()}, indent=2))
        return 0

    if args.command == "openapi":
        print(json.dumps(app.openapi(), indent=2))
        return 0

    if args.command == "security":
        s = app.security_report()
        print(json.dumps(s, indent=2))
        if not s["auth_configured"]:
            print(
                "\n⚠ No authentication is configured. Every route is public.\n"
                "  Add: app.api_key('WEBCORTEX_API_KEY', id='service', scopes=['read'])",
                file=sys.stderr,
            )
        return 0

    if args.command == "tools":
        report = app.check()
        spec = app.openapi()
        out = []
        for path, methods in spec["paths"].items():
            for method, op in methods.items():
                if op.get("x-webcortex-tool"):
                    out.append({
                        "name": op["operationId"],
                        "method": method.upper(),
                        "path": path,
                        "description": op.get("description") or op.get("summary"),
                        "executed_by": op.get("x-webcortex-op"),
                    })
        print(json.dumps({"tools": out, "agents": report["agents"]}, indent=2))
        return 0

    if args.command == "sql":
        print(app.schema_sql or "-- no resources declared")
        return 0

    if args.command == "context":
        if args.json:
            manifest = app.manifest()
            print(json.dumps(manifest if args.show_secrets else redacted_manifest(manifest), indent=2))
        else:
            print(app.context_pack(), end="")
        return 0

    if args.command == "mcp-config":
        key = os.environ.get("WEBCORTEX_API_KEY")
        cfg = mcp_config(app, key)
        name = next(iter(cfg["mcpServers"]))
        url = cfg["mcpServers"][name]["url"]
        print(json.dumps(cfg, indent=2))
        print(f"\n# Claude Code:\n{claude_mcp_add_command(name, url, key)}")
        if not key:
            print("# (set WEBCORTEX_API_KEY to fill the header in)", file=sys.stderr)
        return 0

    if args.command == "evolve":
        return _cmd_evolve(app, args)


    if args.command == "typegen":
        code = app.typescript_client()
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(code)
        methods = code.count("): Promise<")
        print(f"Wrote {out} ({methods} typed methods, {len(code.splitlines())} lines)")
        return 0

    parser.error(f"unknown command {args.command}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
