"""The context pack: an application, described for a model.

An AI coding tool working on a WebCortex app should not have to read the
framework's source, the docs site and every file in the project to add a
route correctly. It needs the app's *shape* — what exists, what it is called,
what it accepts, who may call it — and the handful of framework signatures
that matter, in a few thousand tokens.

That is what `webcortex context` prints and what `webcortex evolve` feeds to
a model. The pack is derived from the manifest, so it cannot drift from the
code, and it is deliberately terse: every line here is re-sent on every turn
of whatever conversation it lands in.
"""

from __future__ import annotations

import json
from typing import Any

# The framework's surface, compressed. Kept here rather than in the docs so
# the model sees signatures that match the installed version.
CHEATSHEET = """\
## WebCortex 2 — how to extend this app

Declare, don't handle: anything expressible as data runs in Rust and is
automatically a REST route, an OpenAPI operation and an MCP tool.

app = WebCortex(name, *, database="sqlite://./app.db", templates=None, port=8000)

# security (deny by default; relaxing costs a line)
app.api_key("ENV_VAR", id="who", scopes=[...]);  app.jwt(secret_env="ENV_VAR")
app.anonymous_scopes(*scopes);  app.rate_limit(per_second=50, burst=100);  app.cors(*origins)

# routes — executed in Rust
app.resource("things", fields={"id": int, "name": str}, tools=True, read_scopes=[...], write_scopes=[...])
app.query("GET", "/x/{id}", "SELECT ... WHERE id = ?", params=["id"], returns="one|many|affected", tool=True, tool_name="...", scopes=[...])
app.static("GET", "/ping", {"ok": True});  app.static_files("/assets", "static")
app.upstream("name", base_url, bearer_env="ENV");  app.proxy("GET", "/p/{x}", upstream="name", rewrite="/y/{x}")
app.page("/", "index.html", sql="SELECT ...", bind="rows")

# routes — Python (parameters bind path -> query -> body; signature = tool schema)
@app.get("/things/{id}/summary", tool=True, scopes=["read"], approval="never|required")
def summary(id: int, style: str = "short") -> dict: '''Docstring = tool description.'''

# models: tiers and providers (prefix picks the wire format: ollama/, openai/, anthropic/, <provider>/)
app.models(default="claude-opus-5", fast="claude-haiku-4-5-20251001", local="ollama/qwen3.5:9b")
app.provider("groq", base_url="https://api.groq.com/openai/v1", api_key_env="GROQ_API_KEY")
app.pricing("claude-opus-5", input_per_mtok=..., output_per_mtok=...)

# context providers (resolved at run start; SQL may bind @principal)
app.context("catalogue", sql="SELECT ...", max_chars=4000);  app.context("policy", data={...})
@app.context("account")
def account(req) -> dict: ...

# memory: four Rust tools, keyed by the root principal
notes = app.memory("notes", scopes=["read"])   # notes_remember/recall/search/forget

# agents (every agent is a tool named after itself; endpoint takes {"input", "session_id"})
app.agent("name", model="default", system="...", tools=[...], handoffs=[...], context=[...], memory="notes",
          scopes=[...], expose_scopes=[...], max_steps=12, token_budget=100_000,
          context_window=None, tool_result_limit=16_384, compact_with="fast", cache=True)

# behaviours: deterministic Python control flow, probabilistic leaves
@app.behaviour("name", tools=[...], context=[...], scopes=[...], max_steps=50, token_budget=None, model="default")
def name(ctx, input):
    rows = ctx.call("list_things", limit=50)                     # one tool, in-process
    many = ctx.gather(("get_things", {"id": 1}), ("get_things", {"id": 2}))   # concurrent
    v = ctx.ask("...", schema={...}, model="fast")               # forced structured output
    vs = ctx.ask_many([...prompts], schema={...}, model="fast")  # concurrent
    ctx.context("catalogue"); ctx.log("..."); ctx.halt("reason"); ctx.usage; ctx.trace; ctx.user

# flows: orchestration as data, executed in Rust (steps are any tools; one shared budget)
app.flow("report", pipeline=["researcher", "writer"], token_budget=200_000)
app.flow("audit", parallel=["a", "b"], merge="collect|merge")
app.flow("desk", route={"billing": "billing_agent", "tech": "tech_agent"}, default="general", classify_with="fast")
# step mapping: {"tool": "x", "input": {"id": "$.id", "q": "$input.query"}}  ($ = incoming, $input = original)

Rules the runtime enforces: agent/behaviour/flow scopes are intersected with the caller's;
approval="required" tools suspend a run (resume via POST /_webcortex/approvals/{id});
max_steps and token_budget are hard limits; nesting is capped at 8; every step is audited.
CLI: webcortex check | security | tools | context | evolve "…" | typegen | openapi | sql | dev
"""


def build(app: Any) -> str:
    """Render the pack as Markdown."""
    m = app.manifest()
    sec = app.security_report()
    lines: list[str] = []
    add = lines.append

    add(f"# {m['name']} — WebCortex context pack")
    if m.get("description"):
        add(m["description"])
    add("")
    meta = [f"version {m['version']}"]
    meta.append(f"database {m['database']['url']}" if m.get("database") else "no database")
    if m.get("templates"):
        meta.append(f"templates in {m['templates']['dir']}/")
    meta.append(f"control plane at {m['server']['control_prefix']}")
    add("; ".join(meta) + ".")
    add("")

    # --- Security -------------------------------------------------------
    add("## Security")
    posture = []
    posture.append("auth: " + (", ".join(
        f"{k} (env {env}, scopes {spec['scopes']})"
        for env, spec in m["auth"]["api_keys"].items() for k in [spec["id"]]
    ) if m["auth"]["api_keys"] else "NONE"))
    if m["auth"].get("jwt"):
        posture.append(f"jwt via {m['auth']['jwt']['secret_env']}")
    if m["auth"]["anonymous_scopes"]:
        posture.append(f"anonymous scopes {m['auth']['anonymous_scopes']}")
    posture.append("rate-limited" if sec["rate_limited"] else "no rate limit")
    posture.append(f"cors {sec['cors_origins']}" if sec["cors_enabled"] else "no cors")
    add("- " + "; ".join(posture))
    add(f"- public routes ({len(sec['public_routes'])}): " + (", ".join(sec["public_routes"][:12]) + (" …" if len(sec["public_routes"]) > 12 else "")))
    if sec["gated_tools"]:
        add(f"- approval-gated tools: {', '.join(sec['gated_tools'])}")
    add("")

    # --- Routes ---------------------------------------------------------
    routes = m["routes"]
    native = sum(1 for r in routes if r["op"]["kind"] != "python")
    add(f"## Routes ({len(routes)}, {native} served in Rust)")
    add("| Method | Path | Engine | Tool | Scopes |")
    add("|---|---|---|---|---|")
    for r in routes:
        tool = r["tool"]["name"] or (_derive_tool_name(r["method"], r["path"]) if r["tool"]["expose"] else "")
        add(f"| {r['method']} | {r['path']} | {r['op']['kind']} | {tool} | {' '.join(r['scopes']) or '-'} |")
    add("")

    # --- Tools ----------------------------------------------------------
    tools = [r for r in routes if r["tool"]["expose"]]
    add(f"## Tools ({len(tools)})")
    for r in tools:
        name = r["tool"]["name"] or _derive_tool_name(r["method"], r["path"])
        desc = (r.get("description") or r.get("summary") or "").strip().split("\n", 1)[0]
        args = _args(r.get("input_schema"))
        gate = " [approval required]" if r.get("approval") == "required" else ""
        add(f"- `{name}({args})` — {desc}{gate}")
    add("")

    # --- Agents ---------------------------------------------------------
    if m["agents"]:
        add(f"## Agents ({len(m['agents'])})")
        for a in m["agents"]:
            bits = [f"model={a['model']}", f"tools={a['tools']}"]
            if a.get("handoffs"):
                bits.append(f"handoffs={a['handoffs']}")
            if a.get("context"):
                bits.append(f"context={a['context']}")
            bits.append(f"scopes={a['scopes']}")
            bits.append(f"max_steps={a['max_steps']}")
            if a.get("token_budget"):
                bits.append(f"token_budget={a['token_budget']}")
            pol = a.get("policy") or {}
            if pol.get("max_context_tokens"):
                bits.append(f"context_window={pol['max_context_tokens']}")
            add(f"- **{a['name']}** — {a.get('description') or '(no description)'}")
            add(f"  {'; '.join(bits)}")
            if a.get("system"):
                add(f"  system: {_short(a['system'], 160)}")
        add("")

    # --- Behaviours -----------------------------------------------------
    if m["behaviours"]:
        add(f"## Behaviours ({len(m['behaviours'])})")
        for b in m["behaviours"]:
            desc = (b.get("description") or "").strip().split("\n", 1)[0]
            add(f"- **{b['name']}** — {desc or '(no description)'}")
            bits = [f"tools={b['tools']}", f"max_steps={b['max_steps']}", f"model={b['model']}"]
            if b.get("context"):
                bits.append(f"context={b['context']}")
            if b.get("token_budget"):
                bits.append(f"token_budget={b['token_budget']}")
            add(f"  {'; '.join(bits)}; input: {_args(b.get('input_schema'))}")
        add("")

    # --- Flows ----------------------------------------------------------
    if m.get("flows"):
        add(f"## Flows ({len(m['flows'])})")
        for f in m["flows"]:
            k = f["kind"]
            if k["kind"] == "pipeline":
                shape = " → ".join(s["tool"] for s in k["steps"])
            elif k["kind"] == "parallel":
                shape = f"parallel [{', '.join(s['tool'] for s in k['branches'])}] merge={k['merge']}"
            else:
                routes_ = ", ".join(f"{label}→{s['tool']}" for label, s in k["routes"].items())
                shape = f"route {{{routes_}}}"
                if k.get("default"):
                    shape += f" default={k['default']['tool']}"
                shape += f" classify_with={k.get('classify_with') or 'fast'}"
            budget = f"; token_budget={f['token_budget']}" if f.get("token_budget") else ""
            add(f"- **{f['name']}** — {f.get('description') or shape}")
            add(f"  {shape}; scopes={f['scopes']}{budget}")
        add("")

    # --- Context providers ---------------------------------------------
    if m.get("contexts"):
        add(f"## Context providers ({len(m['contexts'])})")
        for c in m["contexts"]:
            add(f"- **{c['name']}** ({c['source']['kind']}, max {c['max_chars']} chars) — {c.get('description') or ''}")
        add("")

    # --- Memory ---------------------------------------------------------
    mems = getattr(app, "_memories", [])
    if mems:
        add("## Memory")
        for mem in mems:
            add(f"- **{mem['name']}** → tools {mem['tools']} (table {mem['table']}, per root principal)")
        add("")

    # --- Models ---------------------------------------------------------
    models = m.get("models") or {}
    if models.get("aliases") or models.get("providers") or models.get("pricing"):
        add("## Models")
        if models.get("aliases"):
            add("- aliases: " + ", ".join(f"{k}={v}" for k, v in models["aliases"].items()))
        if models.get("providers"):
            add("- providers: " + ", ".join(f"{k}→{v['base_url']}" for k, v in models["providers"].items()))
        if models.get("pricing"):
            add("- priced: " + ", ".join(models["pricing"]))
        add("")

    add(CHEATSHEET.rstrip())
    return "\n".join(lines) + "\n"


def _args(schema: dict | None) -> str:
    if not schema or not isinstance(schema, dict):
        return ""
    props = schema.get("properties") or {}
    required = set(schema.get("required") or [])
    out = []
    for name, spec in props.items():
        typ = _type_of(spec)
        out.append(f"{name}: {typ}" if name in required else f"{name}?: {typ}")
    return ", ".join(out)


def _type_of(spec: Any) -> str:
    if not isinstance(spec, dict):
        return "any"
    if "enum" in spec:
        return "|".join(json.dumps(v) for v in spec["enum"][:6])
    t = spec.get("type")
    if t == "array":
        return f"{_type_of(spec.get('items'))}[]"
    if t == "object" and spec.get("properties"):
        return "{" + _args(spec) + "}"
    if isinstance(t, list):
        return "|".join(t)
    return t or "any"


def _short(text: str, n: int) -> str:
    text = " ".join(text.split())
    return text if len(text) <= n else text[: n - 1] + "…"


def _derive_tool_name(method: str, path: str) -> str:
    verb = {"GET": "get", "POST": "create", "PUT": "update", "PATCH": "update", "DELETE": "delete"}.get(method, method.lower())
    parts = [verb]
    for seg in path.split("/"):
        if not seg:
            continue
        if seg.startswith("{") and seg.endswith("}"):
            parts += ["by", seg[1:-1].replace("-", "_")]
        else:
            parts.append(seg.replace("-", "_"))
    return "_".join(parts)
