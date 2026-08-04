"""The `pylon` command line.

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
            raise SystemExit(f"pylon: cannot load {path}")
        module = importlib.util.module_from_spec(spec)
        sys.modules[path.stem] = module
        spec.loader.exec_module(module)
    else:
        module = importlib.import_module(module_part)

    if not hasattr(module, attr):
        raise SystemExit(
            f"pylon: {module_part} does not define {attr!r}. "
            f"Define `{attr} = Pylon(...)` or pass module:name."
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

    print(f"  pylon {app.version}  ·  {app.name}")
    print(f"  python {sys.version.split()[0]} ({gil})")
    print(f"  {report['routes']} routes, {report['native_routes']} served without touching Python")

    if report["tools"]:
        shown = ", ".join(report["tools"][:5])
        more = f" … +{len(report['tools']) - 5}" if len(report["tools"]) > 5 else ""
        print(f"  {len(report['tools'])} agent tools: {shown}{more}")
    if report["agents"]:
        print(f"  agents: {', '.join(report['agents'])}")

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
              f"(run `pylon security` to list them)")
    if security["gated_tools"]:
        print(f"  approval-gated tools: {', '.join(security['gated_tools'])}")

    prefix, host, port = app.control_prefix, app.host, app.port
    print(f"  http://{host}:{port}{prefix}/openapi.json   ·   MCP: http://{host}:{port}{prefix}/mcp")
    print()


def _cmd_new(args: argparse.Namespace) -> int:
    from . import starters

    name = args.name
    target = Path(args.directory or name)
    if target.exists() and any(target.iterdir()):
        raise SystemExit(f"pylon: {target}/ already exists and is not empty")

    try:
        files = starters.files_for(args.template, name, args.description or f"A Pylon app: {name}")
    except ValueError as e:
        raise SystemExit(f"pylon: {e}")

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
        f"  export PYLON_API_KEY=$(pylon keygen)\n"
        f"  pylon dev\n"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="pylon", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    new = sub.add_parser("new", help="scaffold a new project")
    new.add_argument("name")
    new.add_argument(
        "--template", "-t", default="api",
        choices=("api", "fullstack", "agent"),
        help="api: JSON+MCP · fullstack: adds pages · agent: adds an agent with an approval gate",
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
    ]:
        p = sub.add_parser(name, help=help_text)
        p.add_argument("target", nargs="?", default=DEFAULT_MODULE,
                       help="module path or module:attr (default: api.py)")
        if name in ("dev", "run"):
            p.add_argument("--host", default=None)
            p.add_argument("--port", type=int, default=None)
            p.add_argument("--workers", type=int, default=None)
        if name == "typegen":
            p.add_argument("--out", "-o", default="client/api.ts")

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
            os.environ.setdefault("PYLON_LOG", "info")
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
                "  Add: app.api_key('PYLON_API_KEY', id='service', scopes=['read'])",
                file=sys.stderr,
            )
        return 0

    if args.command == "tools":
        report = app.check()
        spec = app.openapi()
        out = []
        for path, methods in spec["paths"].items():
            for method, op in methods.items():
                if op.get("x-pylon-tool"):
                    out.append({
                        "name": op["operationId"],
                        "method": method.upper(),
                        "path": path,
                        "description": op.get("description") or op.get("summary"),
                        "executed_by": op.get("x-pylon-op"),
                    })
        print(json.dumps({"tools": out, "agents": report["agents"]}, indent=2))
        return 0

    if args.command == "sql":
        print(app.schema_sql or "-- no resources declared")
        return 0

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
