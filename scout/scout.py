#!/usr/bin/env python3
"""The scout: a local model reads this repository and leaves suggestions.

The framework's own development loop, written down. On each run the scout:

  1. checks that a local Ollama server is up (otherwise it exits quietly);
  2. gathers what changed since it last looked, plus one rotating *focus area*
     of the codebase so the whole repository is revisited over time;
  3. asks the model, one file at a time, for concrete improvements in a fixed
     JSON shape — a deterministic loop with probabilistic leaves, exactly like
     a Behaviour;
  4. de-duplicates against what it already suggested and appends the rest to
     `scout/suggestions.md`, dated and tied to a commit hash.

A human — or a coding agent told to "run the scout suggestions" — reads that
file and decides. The scout never edits code, never commits, and never talks
to anything but the local model.

Standard library only, so it runs from cron or launchd with no environment to
set up. Configuration is by environment variable:

  SCOUT_MODEL     ollama model to use            (default: qwen3.5:9b)
  OLLAMA_HOST     where Ollama listens           (default: http://127.0.0.1:11434)
  SCOUT_MAX_FILES how many files to review/run   (default: 8)
  SCOUT_REPO      repository root                (default: the parent of this file)
"""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = Path(os.environ.get("SCOUT_REPO", HERE.parent)).resolve()
MODEL = os.environ.get("SCOUT_MODEL", "qwen3.5:9b")
HOST = os.environ.get("OLLAMA_HOST", "http://127.0.0.1:11434").rstrip("/")
if not HOST.startswith("http"):
    HOST = f"http://{HOST}"
MAX_FILES = int(os.environ.get("SCOUT_MAX_FILES", "8"))

STATE = HERE / "state.json"
OUT = HERE / "suggestions.md"
LOG = HERE / "logs"

# The rotation. Each run looks at changed files first, then fills up to
# MAX_FILES from the next area, so over a week the whole repository is read.
AREAS: list[tuple[str, list[str]]] = [
    ("agent runtime", ["crates/webcortex-core/src/agent.rs"]),
    ("providers and registry", ["crates/webcortex-core/src/agent/provider.rs",
                                "crates/webcortex-core/src/agent/registry.rs"]),
    ("flows and context", ["crates/webcortex-core/src/flow.rs",
                           "crates/webcortex-core/src/context.rs",
                           "crates/webcortex-core/src/ledger.rs"]),
    ("dispatcher and server", ["crates/webcortex-core/src/app.rs",
                               "crates/webcortex-core/src/server.rs"]),
    ("security", ["crates/webcortex-core/src/auth.rs",
                  "crates/webcortex-core/src/middleware.rs",
                  "crates/webcortex-core/src/mcp.rs"]),
    ("manifest and validation", ["crates/webcortex-core/src/manifest.rs"]),
    ("behaviour bridge", ["crates/webcortex-py/src/behaviour.rs",
                          "crates/webcortex-py/src/lib.rs"]),
    ("python api", ["python/webcortex/app.py"]),
    ("python tooling", ["python/webcortex/cli.py", "python/webcortex/contextpack.py",
                        "python/webcortex/_bridge.py", "python/webcortex/schema.py"]),
    ("starters and example", ["python/webcortex/starters.py", "examples/hello/api.py"]),
    ("tests", ["tests/test_orchestration.py", "tests/test_behaviours.py",
               "tests/test_pentest.py"]),
    ("docs: agents", ["docs/agents.md", "docs/orchestration.md", "docs/context.md",
                      "docs/models.md"]),
    ("docs: getting started", ["README.md", "docs/index.md", "docs/tutorial.md",
                               "docs/ai-development.md"]),
    ("design", ["DESIGN.md", "AGENTS.md", "SECURITY.md"]),
]

SYSTEM = """\
You are reviewing one file from WebCortex, a Python web framework with a Rust
core where every declared route is also an agent tool, and where agents,
behaviours and flows compose under one shared token budget. Its stated goals:
be the framework for agentic development; make agent orchestration accurate
and safe; spend as few tokens as possible; give models excellent context; and
be easy for an AI coding tool to extend.

Propose improvements to this file that serve those goals. Be concrete: name
the function or section, say what is wrong or missing, and say exactly what
to change. Prefer a few sharp suggestions over many vague ones. Do not
suggest rewriting in another language, adding an ORM, embedding inference, or
building a universal LLM abstraction — those are deliberately out of scope.
Do not flag style. If the file is fine, return an empty list.
"""

SCHEMA = {
    "type": "object",
    "properties": {
        "suggestions": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "area": {"type": "string", "enum": [
                        "orchestration", "token-economy", "context", "security",
                        "correctness", "performance", "developer-experience", "docs",
                    ]},
                    "severity": {"type": "string", "enum": ["low", "medium", "high"]},
                    "where": {"type": "string"},
                    "rationale": {"type": "string"},
                    "proposal": {"type": "string"},
                },
                "required": ["title", "area", "severity", "where", "rationale", "proposal"],
            },
        }
    },
    "required": ["suggestions"],
}

MAX_CHARS = 14_000  # per chunk sent to the model


def log(msg: str) -> None:
    print(f"[scout {dt.datetime.now():%Y-%m-%d %H:%M:%S}] {msg}", flush=True)


def ollama_up() -> bool:
    try:
        with urllib.request.urlopen(f"{HOST}/api/tags", timeout=3) as r:
            tags = json.loads(r.read())
    except Exception:
        return False
    names = {m.get("name") for m in tags.get("models", [])}
    if MODEL not in names and f"{MODEL}:latest" not in names:
        log(f"model {MODEL!r} is not pulled; available: {sorted(n for n in names if n)}")
        return False
    return True


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=REPO, capture_output=True, text=True, check=False).stdout.strip()


def load_state() -> dict:
    if STATE.exists():
        try:
            return json.loads(STATE.read_text())
        except Exception:
            pass
    return {"last_commit": None, "area_index": 0, "seen": []}


def save_state(state: dict) -> None:
    STATE.write_text(json.dumps(state, indent=2) + "\n")


def changed_files(since: str | None) -> list[str]:
    if not since:
        return []
    out = git("diff", "--name-only", f"{since}..HEAD")
    return [f for f in out.splitlines() if (REPO / f).is_file()]


def pick_files(state: dict, head: str) -> tuple[list[str], str]:
    """Changed files first, then the next area in the rotation."""
    files: list[str] = []
    for f in changed_files(state.get("last_commit")):
        if f.endswith((".rs", ".py", ".md")) and f not in files:
            files.append(f)
    area_name, area_files = AREAS[state.get("area_index", 0) % len(AREAS)]
    for f in area_files:
        if len(files) >= MAX_FILES:
            break
        if f not in files and (REPO / f).is_file():
            files.append(f)
    return files[:MAX_FILES], area_name


def chunks(text: str) -> list[str]:
    if len(text) <= MAX_CHARS:
        return [text]
    out, buf = [], []
    size = 0
    for line in text.splitlines(keepends=True):
        if size + len(line) > MAX_CHARS and buf:
            out.append("".join(buf))
            buf, size = [], 0
        buf.append(line)
        size += len(line)
    if buf:
        out.append("".join(buf))
    return out


def ask(path: str, chunk: str, part: int, parts: int) -> list[dict]:
    """One model call with a forced JSON shape — the probabilistic leaf."""
    prompt = (
        f"File: {path}" + (f" (part {part} of {parts})" if parts > 1 else "") +
        "\n\n```\n" + chunk + "\n```\n\n"
        'Return JSON of the form {"suggestions": [{"title", "area", "severity", '
        '"where", "rationale", "proposal"}]}. An empty list means the file is fine.'
    )
    body = {
        "model": MODEL,
        "messages": [{"role": "system", "content": SYSTEM}, {"role": "user", "content": prompt}],
        "format": SCHEMA,
        "stream": False,
        "think": False,
        "options": {"temperature": 0.2, "num_ctx": 32768},
    }
    req = urllib.request.Request(
        f"{HOST}/api/chat", data=json.dumps(body).encode(),
        headers={"content-type": "application/json"}, method="POST",
    )
    # Anything the network or the server can do wrong is a skipped chunk, not a
    # crashed run: a dropped connection, a timeout, a 4xx for an option the
    # server does not know, a body that is not JSON.
    try:
        with urllib.request.urlopen(req, timeout=900) as r:
            payload = json.loads(r.read())
    except urllib.error.HTTPError as e:
        detail = e.read()[:300].decode("utf-8", "replace")
        log(f"model call failed for {path}: HTTP {e.code}: {detail}")
        return []
    except Exception as e:  # noqa: BLE001 - a scheduled reviewer must not die on one bad call
        log(f"model call failed for {path}: {type(e).__name__}: {e}")
        return []

    content = payload.get("message", {}).get("content", "")
    try:
        parsed = json.loads(content)
    except json.JSONDecodeError:
        m = re.search(r"[\[{].*[\]}]", content, re.S)
        if not m:
            log(f"unparseable answer for {path}: {content[:120]!r}")
            return []
        try:
            parsed = json.loads(m.group(0))
        except json.JSONDecodeError:
            log(f"unparseable answer for {path}: {content[:120]!r}")
            return []
    items = normalise(parsed)
    if not items and content.strip() not in ("", "[]", '{"suggestions": []}', '{"suggestions":[]}'):
        log(f"no usable suggestions in answer for {path}; it began {content[:100]!r}")
    for it in items:
        it["file"] = path
    return items


_SEVERITIES = ("low", "medium", "high")


def normalise(parsed: object) -> list[dict]:
    """Accept the shapes a small model actually emits.

    The schema asks for `{"suggestions": [...]}` with fixed keys; local models
    frequently answer with a bare list, or with `section` / `issue` / `fix`
    instead of `where` / `rationale` / `proposal`. Mapping those here is
    cheaper than losing the run.
    """
    if isinstance(parsed, dict):
        items = parsed.get("suggestions")
        if items is None:
            items = [parsed] if any(k in parsed for k in ("title", "issue", "proposal", "fix")) else []
    elif isinstance(parsed, list):
        items = parsed
    else:
        items = []
    if not isinstance(items, list):
        return []

    out: list[dict] = []
    for it in items:
        if not isinstance(it, dict):
            continue
        proposal = (it.get("proposal") or it.get("fix") or it.get("suggestion")
                    or it.get("recommendation") or it.get("change") or "")
        rationale = it.get("rationale") or it.get("issue") or it.get("problem") or it.get("why") or ""
        where = it.get("where") or it.get("section") or it.get("location") or it.get("function") or ""
        title = it.get("title") or ""
        if not title:
            head = (rationale or proposal).strip().split(". ", 1)[0]
            title = (f"{where}: {head}" if where else head)[:120]
        if not title or not str(proposal).strip():
            continue
        severity = str(it.get("severity", "")).lower()
        out.append({
            "title": str(title).strip(),
            "area": str(it.get("area") or "correctness"),
            "severity": severity if severity in _SEVERITIES else "medium",
            "where": str(where),
            "rationale": str(rationale).strip(),
            "proposal": str(proposal).strip(),
        })
    return out



def fingerprint(s: dict) -> str:
    key = re.sub(r"[^a-z0-9]+", " ", (s["file"] + " " + s["title"]).lower()).strip()
    return hashlib.sha1(key.encode()).hexdigest()[:12]


def render(suggestions: list[dict], head: str, area: str, files: list[str]) -> str:
    stamp = dt.datetime.now().strftime("%Y-%m-%d %H:%M")
    lines = [f"## {stamp} — commit `{head[:10]}` — focus: {area}", "",
             f"Reviewed: {', '.join(f'`{f}`' for f in files)}", ""]
    if not suggestions:
        lines.append("_No new suggestions._")
        lines.append("")
        return "\n".join(lines)
    order = {"high": 0, "medium": 1, "low": 2}
    for s in sorted(suggestions, key=lambda x: (order.get(x.get("severity", "low"), 3), x["file"])):
        lines.append(f"### [{s.get('severity', 'low')}] {s['title']}")
        lines.append(f"- **File:** `{s['file']}` — {s.get('where', '')}")
        lines.append(f"- **Area:** {s.get('area', '')}")
        lines.append(f"- **Why:** {s.get('rationale', '').strip()}")
        lines.append(f"- **Proposal:** {s.get('proposal', '').strip()}")
        lines.append(f"- **Status:** open  <!-- id:{fingerprint(s)} -->")
        lines.append("")
    return "\n".join(lines)


HEADER = """\
# Scout suggestions

Written by `scout/scout.py` using a local model. Newest run last. Each entry
is a proposal, not a decision: read it, and either implement it (then mark it
`done`), reject it (mark it `rejected`, ideally with a word on why), or leave
it `open`. An AI coding tool can be pointed at this file with
"run the scout suggestions".

Entries are de-duplicated by file and title across runs, so a rejected idea
does not come back unless the file changes materially.

"""


def main() -> int:
    LOG.mkdir(exist_ok=True)
    if not ollama_up():
        log(f"ollama at {HOST} is not serving {MODEL!r}; skipping this run")
        return 0

    head = git("rev-parse", "HEAD")
    if not head:
        log("not a git repository; nothing to do")
        return 0

    state = load_state()
    files, area = pick_files(state, head)
    log(f"reviewing {len(files)} file(s), focus {area!r}, model {MODEL}")

    seen: set[str] = set(state.get("seen", []))
    fresh: list[dict] = []
    for path in files:
        text = (REPO / path).read_text(errors="replace")
        parts = chunks(text)
        for i, chunk in enumerate(parts, 1):
            for s in ask(path, chunk, i, len(parts)):
                fp = fingerprint(s)
                if fp in seen:
                    continue
                seen.add(fp)
                fresh.append(s)
        log(f"  {path}: {sum(1 for s in fresh if s['file'] == path)} new")

    if not OUT.exists():
        OUT.write_text(HEADER)
    with OUT.open("a") as f:
        f.write(render(fresh, head, area, files))
        f.write("\n")

    state["last_commit"] = head
    state["area_index"] = (state.get("area_index", 0) + 1) % len(AREAS)
    state["seen"] = sorted(seen)[-5000:]
    save_state(state)
    log(f"wrote {len(fresh)} suggestion(s) to {OUT.relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
