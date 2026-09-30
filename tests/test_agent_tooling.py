"""What 2.1 ships for the coding agent: the rules in every starter, and one command to plug
the app into an MCP client."""
from __future__ import annotations

import json

import pytest

from webcortex import starters
from webcortex.cli import _load_app, main, mcp_config


@pytest.mark.parametrize("template", starters.TEMPLATES)
def test_every_starter_ships_agent_instructions(template):
    files = starters.files_for(template, "demo", "a demo")
    assert "AGENTS.md" in files and "CLAUDE.md" in files
    agents = files["AGENTS.md"]
    assert agents.startswith("# demo — notes for coding agents")
    for must in ("webcortex context", "webcortex check", "webcortex security", "tool=True", "approval="):
        assert must in agents, must
    assert files["CLAUDE.md"].strip() == "@AGENTS.md"


def _write_starter(tmp_path, template="api"):
    for relative, contents in starters.files_for(template, "demo", "a demo").items():
        path = tmp_path / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)


def test_mcp_config_describes_this_app(tmp_path, monkeypatch):
    _write_starter(tmp_path)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("WEBCORTEX_API_KEY", "test-key-abcdefghijklmnop")
    app = _load_app("api.py")

    cfg = mcp_config(app, key="test-key-abcdefghijklmnop")
    entry = cfg["mcpServers"]["demo"]
    assert entry["type"] == "http"
    assert entry["url"].endswith("/_webcortex/mcp") and entry["url"].startswith("http://")
    assert entry["headers"] == {"x-api-key": "test-key-abcdefghijklmnop"}

    unauthenticated = mcp_config(app, key=None)
    assert unauthenticated["mcpServers"]["demo"]["headers"] == {}


def test_mcp_config_command_prints_json_and_the_claude_line(tmp_path, monkeypatch, capsys):
    _write_starter(tmp_path)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("WEBCORTEX_API_KEY", "test-key-abcdefghijklmnop")
    assert main(["mcp-config", "api.py"]) == 0
    out = capsys.readouterr().out
    body, _, tail = out.partition("\n# Claude Code")
    cfg = json.loads(body)
    assert cfg["mcpServers"]["demo"]["headers"]["x-api-key"] == "test-key-abcdefghijklmnop"
    assert "claude mcp add --transport http demo http://" in tail
    assert '--header "x-api-key: test-key-abcdefghijklmnop"' in tail
