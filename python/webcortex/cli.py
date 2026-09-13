"""The `webcortex` command line.

Convention over configuration: every command defaults to `api.py` in the current
directory and expects it to define `app`.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import sys
from pathlib import Path
from typing import Any

DEFAULT_MODULE = "api.py"


def _load_app(target: str) -> Any:
    """Import a module by path and return its `app`.

    Accepts `api.py`, `api`, or `pkg.mod:name`.
    """
    module_part, _, attr = target.partition(":")
    attr = attr or "app"

    path = Path(module_part)
    if not path.suffix:
        path = path.with_suffix(".py")

    if path.exists():
        # Sibling modules must be importable: an app split across api.py and
        # agents.py is the expected shape, not an edge case.
        directory = str(path.parent.resolve())
        if directory not in sys.path:
            sys.path.insert(0, directory)
        spec = importlib.util.spec_from_file_location(path.stem, path)
        if spec is None or spec.loader is None:
            raise SystemExit(f"webcortex: cannot load {path}")
        module = importlib.util.module_from_spec(spec)
        sys.modules[path.stem] = module
        spec.loader.exec_module(module)
    else:
        module = importlib.import_module(module_part)

    if not hasattr(module, attr):
        raise SystemExit(
            f"webcortex: {module_part} does not define {attr!r}. "
            f"Define `{attr} = WebCortex(...)` or pass module:name."
        )
    return getattr(module, attr)


def _load_dotenv() -> None:
    """Load `.env` if present. Existing environment always wins."""
    path = Path(".env")
    if not path.exists():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        os.environ.setdefault(key.strip(), value.strip().strip("'\""))


def _banner(app: Any) -> None:
    from ._bridge import free_threaded

    report = app.check()
    security = app.security_report()
    gil = "free-threaded" if free_threaded() else "GIL-enabled"

    print(f"  webcortex {app.version}  ·  {app.name}")
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


def _cmd_new(args: argparse.Namespace) -> int:
    from . import starters

    name = args.name
    target = Path(args.directory or name)
    if target.exists() and any(target.iterdir()):
        raise SystemExit(f"webcortex: {target}/ already exists and is not empty")

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
        f"  webcortex dev\n"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="webcortex", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    new = sub.add_parser("new", help="scaffold a new project")
    new.add_argument("name")
    new.add_argument(
        "--template", "-t", default="api",
        choices=("api", "fullstack", "agent", "behaviour", "orchestration"),
        help=("api: JSON+MCP · fullstack: adds pages · agent: adds an approval gate · "
              "behaviour: adds programmable procedures · orchestration: handoffs, flows, "
              "memory, context and a local model"),
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
        ("evolve", "ask a model to propose an extension, anchored on the context pack"),
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
            p.add_argument("--json", action="store_true", help="emit the raw manifest instead")
        if name == "evolve":
            p.add_argument("--model", "-m", default="default",
                           help="model or alias, e.g. default, fast, ollama/qwen3.5:9b")
            p.add_argument("--out", "-o", default=None, help="write the proposal to a file")
            p.add_argument("--json", action="store_true",
                           help="ask for {summary, code, notes} as JSON")

    args = parser.parse_args(argv)

    if args.command == "new":
        return _cmd_new(args)

    if args.command == "keygen":
        from . import _core

        print(_core.generate_api_key())
        return 0

    _load_dotenv()
    app = _load_app(args.target)

    if args.command in ("dev", "run"):
        if args.host:
            app.host = args.host
        if args.port:
            app.port = args.port
        if args.workers:
            app.workers = args.workers
        if args.command == "dev":
            os.environ.setdefault("WEBCORTEX_LOG", "info")
        _banner(app)
        app.run()
        return 0

    if args.command == "check":
        report = app.check()
        report.pop("openapi", None)
        print(json.dumps({**report, "security": app.security_report()}, indent=2))
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
            print(json.dumps(app.manifest(), indent=2))
        else:
            print(app.context_pack(), end="")
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
