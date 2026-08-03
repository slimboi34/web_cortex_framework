"""The `pylon` command line.

Convention over configuration: `pylon dev` looks for `api.py` in the current
directory and expects it to define `app`. Everything else is derived from there.
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
        # Make sibling modules importable, which is what a developer expects
        # when their app is split across api.py / agents.py / models.py.
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


def _banner(app: Any) -> None:
    from ._bridge import free_threaded

    report = app.check()
    native = report["native_routes"]
    total = report["routes"]
    gil = "free-threaded" if free_threaded() else "GIL-enabled"

    print(f"  pylon {app.version}  ·  {app.name}")
    print(f"  python {sys.version.split()[0]} ({gil})")
    print(f"  {total} routes, {native} served without touching Python")
    if report["tools"]:
        print(f"  {len(report['tools'])} agent tools: {', '.join(report['tools'][:6])}"
              + (" …" if len(report["tools"]) > 6 else ""))
    if report["agents"]:
        print(f"  agents: {', '.join(report['agents'])}")
    prefix = app.control_prefix
    host, port = app.host, app.port
    print(f"  http://{host}:{port}{prefix}/openapi.json   ·   MCP: http://{host}:{port}{prefix}/mcp")
    print()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="pylon", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    for name, help_text in [
        ("dev", "run the development server"),
        ("run", "run the server"),
        ("check", "validate the application and print its route table"),
        ("openapi", "print the OpenAPI document"),
        ("tools", "print the agent tool manifest"),
        ("sql", "print the DDL for declared resources"),
    ]:
        p = sub.add_parser(name, help=help_text)
        p.add_argument("target", nargs="?", default=DEFAULT_MODULE,
                       help="module path or module:attr (default: api.py)")
        if name in ("dev", "run"):
            p.add_argument("--host", default=None)
            p.add_argument("--port", type=int, default=None)
            p.add_argument("--workers", type=int, default=None)

    args = parser.parse_args(argv)
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
        print(json.dumps(report, indent=2)[:4000])
        return 0

    if args.command == "openapi":
        print(json.dumps(app.openapi(), indent=2))
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

    parser.error(f"unknown command {args.command}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
