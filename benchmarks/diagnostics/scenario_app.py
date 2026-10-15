"""The app the diagnostics benchmark breaks on purpose.

Uses only APIs that exist in both 2.4.0 and 2.5.0, so the same file runs on both.
Started twice by run.py: once with WEBCORTEX_FAKE_PROVIDER=1 (agents answer
offline) and once without it and without any provider key (so the keyless and
dead-provider agents fail the way they would in production). No real model API
is ever called.
"""
import os

from webcortex import WebCortex

app = WebCortex("diagbench", port=int(os.environ.get("BENCH_PORT", "8765")), start_halted=True)
app.api_key("BENCH_ADMIN_KEY", id="admin", scopes=["read", "write", "webcortex:admin"])
app.api_key("BENCH_USER_KEY", id="user", scopes=["read"])
app.cors("https://good.example")

# A public Rust route, for the throughput measurement.
app.static("GET", "/fast", {"ok": True})
app.static("GET", "/ping", {"pong": True}, tool=True, tool_name="ping", scopes=["read"])


@app.post("/notes", tool=True, tool_name="add_note", scopes=["read"])
def add_note(title: str) -> dict:
    return {"title": title}


@app.post("/arm/move", tool=True, tool_name="move", actuator=True, scopes=["write"], approval="required")
def move(x: int = 0) -> dict:
    return {"moved": x}


@app.get("/boom", scopes=["read"])
def boom() -> dict:
    raise RuntimeError("a bug in a handler")


app.camera("cam", read_scopes=["read"], ingest_scopes=["write"])

app.provider("deadprov", base_url="http://127.0.0.1:9/v1")
app.agent("desk", tools=["ping", "add_note"], scopes=["read"], token_budget=100_000)
app.agent("tight", tools=["ping"], scopes=["read"], token_budget=1)
app.agent("keyless", model="claude-opus-5", scopes=["read"], token_budget=1000)
app.agent("deadagent", model="deadprov/some-model", scopes=["read"], token_budget=1000)
