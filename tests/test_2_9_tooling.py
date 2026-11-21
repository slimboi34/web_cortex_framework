"""2.9: the Python tooling — evals, the CLI, the starters and `webcortex doctor`.

Regression tests for what the 2.9.0 stress test found in them: eval checks that
passed on what they could not see, cassette errors that printed tracebacks, a
starter placeholder that worked as an admin key, credentials in `context
--json`, an unquoted `claude mcp add` line, boot failures as tracebacks, a
route table `check` promised and did not print, an e-stop that could not reach
an https deployment, a silent bad OTLP endpoint, and a starter eval that could
not pass. Everything runs offline against the fake provider.
"""

from __future__ import annotations

import http.server
import json
import os
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

import pytest

from webcortex import WebCortex, cli, evals, starters
from webcortex.diagnostics import boot_error_code, doctor, is_placeholder_secret


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _isolate(monkeypatch, *names: str) -> None:
    """Unset ``names`` for this test, and undo whatever the CLI's .env loading
    sets on them (``monkeypatch.delenv`` alone records nothing for an unset
    variable, so a value loaded later would leak into other tests)."""
    for name in names:
        monkeypatch.setenv(name, "x")
        monkeypatch.delenv(name)


@pytest.fixture
def fake(monkeypatch):
    _isolate(monkeypatch, "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "WEBCORTEX_API_KEY")
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")


@pytest.fixture
def offline(monkeypatch):
    """No provider at all: no key and no fake."""
    _isolate(monkeypatch, "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "WEBCORTEX_API_KEY", "WEBCORTEX_FAKE_PROVIDER")


def _not_root() -> bool:
    """File permissions bind this process (root reads and writes anything)."""
    return getattr(os, "geteuid", lambda: 0)() != 0


def _write(directory: Path, files: dict[str, str]) -> None:
    for relative, text in files.items():
        path = directory / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def _scaffold(tmp_path: Path, template: str) -> Path:
    target = tmp_path / "proj"
    _write(target, starters.files_for(template, "proj", "a starter"))
    return target


# ======================================================================
# M11: trace-derived eval checks fail closed
# ======================================================================

NESTED = 'tool:billing {"input": "tool:issue_refund {\\"id\\": 7}"}'


def _refund_app(**tracing) -> WebCortex:
    app = WebCortex("m11", database="sqlite://:memory:")
    if tracing:
        app.tracing(**tracing)

    @app.post("/issue_refund", tool=True, tool_name="issue_refund")
    def issue_refund(id: int = 0) -> dict:
        return {"refunded": id}

    @app.get("/ping", tool=True, tool_name="ping")
    def ping() -> dict:
        return {"pong": True}

    app.agent("billing", tools=["issue_refund"], description="billing")
    app.agent("front_desk", tools=["billing"])
    # The front desk delegates to billing, which calls issue_refund: four model calls in the tree.
    app.eval("untouched", target="front_desk", input=NESTED, expect={"tools_not_called": ["issue_refund"]})
    app.eval("one_call", target="front_desk", input=NESTED, expect={"max_model_calls": 1})
    app.eval("called", target="front_desk", input=NESTED, expect={"tools_called": ["billing", "issue_refund"]})
    # A plain route runs no model and calls no tool, and records no trace: that is an answer.
    app.eval("route", target="ping", input={}, expect={"tools_not_called": "issue_refund", "max_model_calls": 0})
    return app


def _by_name(report: dict) -> dict:
    return {r["name"]: r for r in report["results"]}


def test_trace_checks_see_a_nested_call_when_tracing_is_on(fake):
    results = _by_name(_refund_app().run_evals(mode="live"))
    assert results["untouched"]["failures"] == ["tool 'issue_refund' was called"]
    assert results["one_call"]["failures"] == ["made 4 model calls, more than max_model_calls=1"]
    assert results["called"]["ok"], results["called"]["failures"]
    assert results["route"]["ok"], results["route"]["failures"]


def test_trace_checks_fail_closed_when_tracing_is_disabled(fake):
    results = _by_name(_refund_app(enabled=False).run_evals(mode="live"))
    for name, key in [("untouched", "tools_not_called"), ("one_call", "max_model_calls"),
                      ("called", "tools_called")]:
        r = results[name]
        assert not r["ok"], f"{name} passed on a run it could not see"
        assert r["failures"] == [f"cannot check {key}: tracing is disabled (app.tracing(enabled=False)), "
                                 f"and these checks read the run's trace"]
    # The outer agent's own steps used to answer for the whole tree: they no longer do.
    assert [s.get("tool") for s in results["called"]["result"]["steps"] if s.get("tool")] == ["billing"]
    # With tracing off, not even a plain route can show that it called nothing: closed, too.
    route = results["route"]
    assert not route["ok"] and "tracing is disabled" in route["failures"][0]


def test_trace_checks_fail_closed_when_the_trace_is_truncated(fake):
    results = _by_name(_refund_app(max_spans=3).run_evals(mode="live"))
    for name in ("untouched", "one_call", "called"):
        r = results[name]
        assert not r["ok"], f"{name} passed on a truncated trace"
        assert "the trace was truncated" in r["failures"][0] and "max_spans" in r["failures"][0]
        assert "span(s)" in r["failures"][0]


def test_check_result_says_why_the_trace_cannot_answer():
    case = evals.EvalCase("c", target="desk", args={},
                          expect={"tools_called": "a", "tools_not_called": ["b"], "max_model_calls": 1})
    run = {"run_id": "r", "status": "completed", "output": "", "steps": [
        {"kind": "tool_call", "tool": "a"}]}
    # A run envelope with no trace: its own steps are not the request tree.
    assert evals.check_result(case, {"status": 200, "error": None, "usage": {}, "body": run}) == [
        "cannot check tools_called, tools_not_called, max_model_calls: the run recorded no trace"]
    off = {"status": 200, "error": None, "usage": {}, "body": run, "tracing": False}
    assert "tracing is disabled" in evals.check_result(case, off)[0]
    cut = {"status": 200, "error": None, "usage": {}, "body": run,
           "trace": {"model_calls": 1, "dropped_spans": 2, "spans": [{"kind": "tool", "name": "tool a"}]}}
    assert "2 span(s)" in evals.check_result(case, cut)[0]
    whole = dict(cut, trace={"model_calls": 1, "dropped_spans": 0, "spans": [{"kind": "tool", "name": "tool a"}]})
    assert evals.check_result(case, whole) == []
    # Checks that do not read the trace are unaffected by its absence.
    plain = evals.EvalCase("c", target="desk", args={}, expect={"output_contains": "x"})
    assert evals.check_result(plain, {"status": 200, "error": None, "usage": {},
                                      "body": {**run, "output": "x"}, "tracing": False}) == []


# ======================================================================
# L25 + L26 + L27: data equality, refused calls, expect types
# ======================================================================


def test_data_checks_tell_true_from_1_and_false_from_0(fake):
    app = WebCortex("l25", database="sqlite://:memory:")

    @app.post("/decide", tool=True, tool_name="decide")
    def decide(id: int = 0) -> dict:
        return {"approved": 1, "count": False, "ratio": 1, "flags": [1, 0], "nested": {"on": True}}

    app.agent("free", output={"type": "object", "properties": {"approved": {}}, "required": ["approved"]})
    app.eval("true_vs_1", target="decide", input={"id": 1}, expect={"data": {"approved": True}})
    app.eval("0_vs_false", target="decide", input={"id": 1}, expect={"data": {"count": 0}})
    app.eval("list_of_bools", target="decide", input={"id": 1}, expect={"data": {"flags": [True, False]}})
    app.eval("agent_true_vs_1", target="free", input='tool:final_answer {"approved": 1}',
             expect={"data": {"approved": True}})
    app.eval("int_vs_float", target="decide", input={"id": 1},
             expect={"data": {"ratio": 1.0, "flags": [1, 0.0], "nested": {"on": True}}})
    results = _by_name(app.run_evals(mode="live"))
    assert results["true_vs_1"]["failures"] == ["data.approved is 1, expected True"]
    assert results["0_vs_false"]["failures"] == ["data.count is False, expected 0"]
    assert results["list_of_bools"]["failures"] == ["data.flags is [1, 0], expected [True, False]"]
    assert results["agent_true_vs_1"]["failures"] == ["data.approved is 1, expected True"]
    assert results["int_vs_float"]["ok"], results["int_vs_float"]["failures"]


def test_a_call_refused_for_a_missing_scope_does_not_count_as_called(fake):
    app = WebCortex("l26", database="sqlite://:memory:")
    ran = []

    @app.post("/issue_refund", tool=True, tool_name="issue_refund", scopes=["billing:write"])
    def issue_refund(id: int = 0) -> dict:
        ran.append(id)
        return {"refunded": id}

    app.agent("desk", tools=["issue_refund"])
    app.eval("untouched", target="desk", input='tool:issue_refund {"id": 9}',
             expect={"tools_not_called": ["issue_refund"]})
    app.eval("claimed", target="desk", input='tool:issue_refund {"id": 9}', expect={"tools_called": ["issue_refund"]})
    results = _by_name(app.run_evals(mode="live"))
    step = next(s for s in results["untouched"]["result"]["steps"] if s.get("tool") == "issue_refund")
    assert step["code"] == "WC602" and not ran
    assert results["untouched"]["ok"], results["untouched"]["failures"]
    assert results["claimed"]["failures"] == ["tools called were [], expected ['issue_refund'] in that order"]


@pytest.mark.parametrize("code", ["WC307", "WC317", "WC502", "WC602", "WC601", "WC604"])
def test_every_runtime_refusal_is_not_a_call(code):
    case = evals.EvalCase("c", target="desk", args={}, expect={"tools_not_called": "arm"})
    outcome = {"status": 200, "error": None, "usage": {},
               "body": {"run_id": "r", "status": "completed", "output": "", "steps": []},
               "trace": {"model_calls": 1, "spans": [{"kind": "tool", "name": "tool arm", "code": code}]}}
    assert evals.check_result(case, outcome) == []
    # A tool that ran and then failed (its handler raised) was called.
    outcome["trace"]["spans"][0]["code"] = "WC002"
    assert evals.check_result(case, outcome) == ["tool 'arm' was called"]


@pytest.mark.parametrize("expect, message", [
    ({"max_steps": "2"}, "expect['max_steps'] must be a non-negative integer, not str '2'"),
    ({"max_tokens": 1.5}, "expect['max_tokens'] must be a non-negative integer"),
    ({"max_model_calls": True}, "expect['max_model_calls'] must be a non-negative integer, not bool"),
    ({"max_steps": -1}, "expect['max_steps'] must be a non-negative integer"),
    ({"http_status": "403"}, "expect['http_status'] must be an HTTP status code"),
    ({"http_status": False}, "expect['http_status'] must be an HTTP status code"),
    ({"status": 200}, "expect['status'] must be a string"),
    ({"code": 602}, "expect['code'] must be a string"),
    ({"tools_called": ["a", 3]}, "expect['tools_called'] must be a tool name or a list of tool names"),
    ({"tools_called": {"a", "b"}}, "expect['tools_called'] must be a tool name or a list of tool names"),
    ({"tools_not_called": None}, "expect['tools_not_called'] must be a tool name or a list"),
    ({"path": [["front_desk"]]}, "expect['path'] must be an agent name or a list of agent names"),
    ({"output_contains": {"a": 1}}, "expect['output_contains'] must be a string or a list of strings"),
    ({"output_matches": 5}, "regular expression"),
    ({"data": {"when": object()}}, "expect['data'] must be JSON-serialisable"),
])
def test_wrongly_typed_expect_values_are_refused_at_declaration(expect, message):
    app = WebCortex("l27")
    app.agent("desk")
    with pytest.raises(ValueError) as e:
        app.eval("typo", target="desk", input="hi", expect=expect)
    assert message in str(e.value)


def test_well_typed_expect_values_and_scopes_are_accepted():
    app = WebCortex("l27")
    app.agent("desk")
    app.eval("ok", target="desk", input="hi", scopes=("read",), expect={
        "max_steps": 0, "max_tokens": 10, "max_model_calls": 2, "http_status": 200, "status": "completed",
        "code": "WC402", "tools_called": "a", "tools_not_called": {"b", "c"}, "path": ("desk",),
        "output_contains": ["x", 42], "output_not_contains": "y", "output_matches": r"\d", "data": {"n": [1]}})
    with pytest.raises(ValueError, match="scopes must be a list of scope names"):
        app.eval("bare", target="desk", input="hi", scopes="read")


def test_an_eval_that_raises_fails_alone_and_the_report_survives(fake, tmp_path, monkeypatch, capsys):
    _write(tmp_path, {"api.py": (
        "from webcortex import WebCortex, evals\n"
        "app = WebCortex('boom', database='sqlite://:memory:')\n"
        "app.agent('desk')\n"
        "app.eval('fine', target='desk', input='hi')\n"
        "# Built by hand, past declare()'s checks: the comparison itself raises.\n"
        "app._evals.append(evals.EvalCase('broken', target='desk', args={'input': 'hi'},"
        " expect={'max_steps': '2'}))\n"
        "app.eval('after', target='desk', input='hi')\n")})
    monkeypatch.chdir(tmp_path)
    assert cli.main(["eval", "--json"]) == 1
    report = json.loads(capsys.readouterr().out)
    results = _by_name(report)
    assert (report["ok"], report["total"], report["passed"]) == (False, 3, 2)
    broken = results["broken"]
    assert broken["code"] == "WC320" and broken["error"]["code"] == "WC320"
    assert broken["failures"][0].startswith("the eval could not be run: TypeError:")
    assert results["fine"]["ok"] and results["after"]["ok"]


# ======================================================================
# K7 + K10: every way an eval cannot run is coded, and never a traceback
# ======================================================================

EVAL_APP = """
from webcortex import WebCortex
app = WebCortex("k10", database="sqlite://:memory:")

@app.get("/ping", tool=True, tool_name="ping")
def ping() -> dict:
    return {"pong": True}

app.agent("desk", model="claude-opus-5", tools=["ping"], system="SYSTEM")
app.eval("lookup", target="desk", input='tool:ping {}', expect={"tools_called": "ping"})
"""


def _eval_cli(capsys, *args: str) -> tuple[int, dict]:
    code = cli.main(["eval", *args, "--json"])
    return code, json.loads(capsys.readouterr().out)


def _only(report: dict) -> dict:
    assert report["total"] == 1
    return report["results"][0]


def test_a_cassette_that_cannot_be_read_is_wc320_with_its_path(fake, tmp_path, monkeypatch, capsys):
    _write(tmp_path, {"api.py": EVAL_APP})
    monkeypatch.chdir(tmp_path)
    cassette = tmp_path / "evals" / "lookup.json"

    code, report = _eval_cli(capsys, "--replay")
    result = _only(report)
    assert code == 1 and result["code"] == "WC320" and result["error"]["code"] == "WC320"
    assert "no cassette at evals/lookup.json" in result["failures"][0]
    assert result["error"]["hint"], "the catalogue's hint travels with the code"

    cassette.mkdir(parents=True)
    code, report = _eval_cli(capsys)           # auto: something is there, so it is replayed
    result = _only(report)
    assert code == 1 and result["code"] == "WC320" and result["mode"] == "replay"
    assert result["failures"] == ["the cassette evals/lookup.json is a directory, not a file; remove it and "
                                  "record it again with `webcortex eval --record`"]
    cassette.rmdir()

    cassette.write_bytes(b"\xff\xfe not text")
    result = _only(_eval_cli(capsys)[1])
    assert result["code"] == "WC320" and "evals/lookup.json is not UTF-8 text" in result["failures"][0]

    cassette.write_text("{not json")
    result = _only(_eval_cli(capsys)[1])
    assert result["code"] == "WC320" and "not valid JSON" in result["failures"][0]
    assert "evals/lookup.json" in result["failures"][0]

    if _not_root():
        cassette.write_text("{}")
        cassette.chmod(0)
        try:
            result = _only(_eval_cli(capsys)[1])
        finally:
            cassette.chmod(0o644)
        assert result["code"] == "WC320"
        assert result["failures"] == ["cannot read the cassette evals/lookup.json: permission denied"]


def test_a_cassette_that_cannot_be_written_is_wc320(fake, tmp_path, monkeypatch, capsys):
    _write(tmp_path, {"api.py": EVAL_APP})
    monkeypatch.chdir(tmp_path)
    (tmp_path / "afile").write_text("")
    code, report = _eval_cli(capsys, "--record", "--dir", "afile")
    result = _only(report)
    assert code == 1 and result["code"] == "WC320" and result["cassette"] is None
    assert result["failures"] == ["cannot write the cassette afile/lookup.json: afile is a file, not a directory"]

    (tmp_path / "evals" / "lookup.json").mkdir(parents=True)
    result = _only(_eval_cli(capsys, "--record")[1])
    assert result["failures"] == ["cannot write the cassette evals/lookup.json: it is a directory"]
    (tmp_path / "evals" / "lookup.json").rmdir()

    if _not_root():
        (tmp_path / "evals").chmod(0o555)
        try:
            result = _only(_eval_cli(capsys, "--record")[1])
        finally:
            (tmp_path / "evals").chmod(0o755)
        assert result["code"] == "WC320" and result["failures"][0].startswith(
            "cannot write the cassette evals/lookup.json: ")
        assert list((tmp_path / "evals").iterdir()) == [], "no half-written cassette or temporary file is left"


def test_recording_without_a_provider_writes_nothing_and_says_why(offline, tmp_path, monkeypatch, capsys):
    _write(tmp_path, {"api.py": EVAL_APP})
    monkeypatch.chdir(tmp_path)
    code, report = _eval_cli(capsys, "--record")
    result = _only(report)
    assert code == 1 and result["code"] == "WC301" and result["error"]["code"] == "WC301"
    assert result["failures"][0].startswith('not recorded: model "claude-opus-5" needs ANTHROPIC_API_KEY')
    assert "evals/lookup.json was not written" in result["failures"][0]
    assert not (tmp_path / "evals" / "lookup.json").exists(), "an empty cassette would replay as WC319"


def test_the_other_eval_failures_are_coded(fake, tmp_path, monkeypatch, capsys):
    _write(tmp_path, {"api.py": EVAL_APP})
    monkeypatch.chdir(tmp_path)
    # An unknown --only: the whole command fails, coded, as JSON on stdout.
    assert cli.main(["eval", "--only", "nope", "--json"]) == 1
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "WC320"
    # A stale cassette.
    assert cli.main(["eval", "--record"]) == 0
    capsys.readouterr()
    (tmp_path / "api.py").write_text(EVAL_APP.replace("SYSTEM", "Something else"))
    result = _only(_eval_cli(capsys, "--replay")[1])
    assert result["code"] == "WC319" and result["error"]["code"] == "WC319"
    # An eval whose target is not a tool.
    (tmp_path / "api.py").write_text(EVAL_APP.replace('target="desk"', 'target="ghost"'))
    result = _only(_eval_cli(capsys, "--live")[1])
    assert result["code"] == "WC320" and 'unknown tool "ghost"' in result["failures"][0]
    # A passing eval carries no error.
    (tmp_path / "api.py").write_text(EVAL_APP)
    assert _only(_eval_cli(capsys, "--live")[1])["error"] is None
    # --list reports the cassette recorded above.
    assert cli.main(["eval", "--list", "--json"]) == 0
    assert json.loads(capsys.readouterr().out)["evals"][0]["cassette"] is True


# ======================================================================
# The orchestration starter's eval
# ======================================================================


def test_the_orchestration_starters_eval_says_how_to_record_and_then_replays(offline, tmp_path, monkeypatch,
                                                                             capsys):
    project = _scaffold(tmp_path, "orchestration")
    monkeypatch.chdir(project)
    monkeypatch.setenv("WEBCORTEX_DATABASE_URL", "sqlite://:memory:")
    source = (project / "api.py").read_text()

    # A fresh scaffold, no key: one failure that says exactly what to do.
    code, report = _eval_cli(capsys)
    result = _only(report)
    assert code == 1 and result["name"] == "refunds_reach_billing" and result["code"] == "WC301"
    assert len(result["failures"]) == 1
    message = result["failures"][0]
    for words in ("no cassette at evals/refunds_reach_billing.json", "ANTHROPIC_API_KEY",
                  "webcortex eval --record", "commit evals/", "replays them with no key"):
        assert words in message, (words, message)
    assert not (project / "evals").exists()

    # The same eval passes in replay once a recording exists. No real model is
    # called here: the fake provider is told to hand off, and since the
    # conversation is not part of a cassette's fingerprint, its recording
    # answers the starter's own wording.
    (project / "api.py").write_text(source.replace(
        'input="I was charged twice for my subscription and want a refund."', 'input="handoff:billing"'))
    monkeypatch.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
    assert cli.main(["eval", "--record"]) == 0, capsys.readouterr().out
    capsys.readouterr()
    monkeypatch.delenv("WEBCORTEX_FAKE_PROVIDER")
    (project / "api.py").write_text(source)
    code, report = _eval_cli(capsys, "--replay")
    assert code == 0 and report["ok"], report["results"][0]["failures"]
    assert _only(report)["result"]["path"] == ["front_desk", "billing"]


def test_the_fake_provider_without_a_cassette_is_explained(fake, tmp_path, monkeypatch, capsys):
    project = _scaffold(tmp_path, "orchestration")
    monkeypatch.chdir(project)
    monkeypatch.setenv("WEBCORTEX_DATABASE_URL", "sqlite://:memory:")
    result = _only(_eval_cli(capsys)[1])
    assert not result["ok"]
    assert result["failures"][0] == "path is ['front_desk'], expected ['front_desk', 'billing']"
    assert "the offline fake provider answered" in result["failures"][-1]
    assert "webcortex eval --record" in result["failures"][-1]


# ======================================================================
# L10: no working placeholder key in a starter, and doctor refuses one
# ======================================================================


def test_env_example_ships_no_usable_secret():
    for template in starters.TEMPLATES:
        env = starters.files_for(template, "demo", "a demo")[".env.example"]
        assert "replace-me" not in env
        values = dict(line.split("=", 1) for line in env.splitlines() if line and not line.startswith("#"))
        assert values == {"WEBCORTEX_API_KEY": "", "WEBCORTEX_JWT_SECRET": ""}
        assert "webcortex keygen" in env


def test_placeholder_secrets_are_recognised():
    for value in (None, "", "  ", "replace-me", "replace-me-run-webcortex-keygen", "REPLACE-ME-x"):
        assert is_placeholder_secret(value), value
    assert not is_placeholder_secret("wc_0123456789abcdef0123456789abcdef")


def test_doctor_fails_an_empty_or_placeholder_key(monkeypatch):
    _isolate(monkeypatch, "L10_KEY", "L10_JWT", "WEBCORTEX_FAKE_PROVIDER")
    app = WebCortex("l10", port=_free_port())
    app.api_key("L10_KEY", id="service", scopes=["webcortex:admin"])
    app.jwt(secret_env="L10_JWT")

    def checks() -> dict:
        return {c["id"]: c for c in doctor(app)["checks"]}

    for value, words in [("replace-me-run-webcortex-keygen", "still the placeholder from .env.example"),
                         ("", "is empty")]:
        monkeypatch.setenv("L10_KEY", value)
        monkeypatch.setenv("L10_JWT", value)
        found = checks()
        key = found["api_key:L10_KEY"]
        assert key["status"] == "fail" and key["code"] == "WC106" and words in key["message"]
        assert "replace-me" not in key["message"], "a value is never printed"
        assert found["jwt"]["status"] == "fail" and found["jwt"]["code"] == "WC106"
    monkeypatch.setenv("L10_KEY", "wc_a-real-generated-key-0123456789")
    monkeypatch.setenv("L10_JWT", "a-real-jwt-secret-of-at-least-32-bytes!!")
    found = checks()
    assert found["api_key:L10_KEY"]["status"] == "ok" and "jwt" not in found


def test_a_copied_env_example_leaves_the_starter_locked(tmp_path, monkeypatch, capsys):
    _isolate(monkeypatch, "WEBCORTEX_API_KEY", "WEBCORTEX_JWT_SECRET", "WEBCORTEX_PORT")
    project = _scaffold(tmp_path, "api")
    shutil.copy(project / ".env.example", project / ".env")      # "Copy to .env and fill in."
    monkeypatch.chdir(project)
    assert cli.main(["doctor", "--json"]) == 1
    key = {c["id"]: c for c in json.loads(capsys.readouterr().out)["checks"]}["api_key:WEBCORTEX_API_KEY"]
    assert key["status"] == "fail" and key["code"] == "WC106"


# ======================================================================
# L12: `context --json` does not print credentials
# ======================================================================

SECRET_APP = """
import os
from webcortex import WebCortex
app = WebCortex("weather", database=os.environ.get("L12_DB", "sqlite://:memory:"))
app.api_key("L12_ADMIN", id="admin", scopes=["devices:read"])
app.upstream("wx", "https://user:pw-SECRET-1@api.weather.example", headers={"X-Api-Key": os.environ["WX_KEY"]},
             bearer_env="WX_TOKEN")
app.proxy("GET", "/wx/{city}", upstream="wx", rewrite="/v1/{city}")
app.camera("lobby", source="http://cam:pw-SECRET-2@192.168.1.4/snap.jpg",
           headers={"Authorization": "Basic SECRET-3"})
app.agent("insp", tools=["lobby_snapshot"], scopes=["devices:read"], token_budget=1000)
app.watch("w", device="lobby", agent="insp", input="x", every=15, scopes=["devices:read"],
          webhook="https://hook:pw-SECRET-4@hooks.example/in")
"""


def test_context_json_redacts_credentials_unless_asked(tmp_path, monkeypatch, capsys):
    _write(tmp_path, {"api.py": SECRET_APP})
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("WX_KEY", "wx-live-SECRET-0")
    monkeypatch.setenv("L12_DB", "postgres://app:pw-SECRET-5@db.internal/app")
    assert cli.main(["context", "--json"]) == 0
    out = capsys.readouterr().out
    assert "SECRET" not in out
    manifest = json.loads(out)
    assert manifest["upstreams"]["wx"]["headers"] == {"X-Api-Key": "<redacted>"}
    assert manifest["upstreams"]["wx"]["bearer_env"] == "WX_TOKEN", "a variable's name is not a secret"
    assert manifest["upstreams"]["wx"]["base_url"] == "https://<redacted>@api.weather.example"
    assert manifest["devices"][0]["source"]["headers"] == {"Authorization": "<redacted>"}
    assert manifest["devices"][0]["source"]["url"] == "http://<redacted>@192.168.1.4/snap.jpg"
    assert manifest["watchers"][0]["webhook"]["url"] == "https://<redacted>@hooks.example/in"
    assert manifest["database"]["url"] == "postgres://<redacted>@db.internal/app"

    assert cli.main(["context", "--json", "--show-secrets"]) == 0
    shown = capsys.readouterr().out
    for n in range(6):
        assert f"SECRET-{n}" in shown


# ======================================================================
# L14: the `claude mcp add` line is quoted
# ======================================================================


def test_mcp_config_prints_a_command_a_shell_reads_as_written(tmp_path, monkeypatch, capsys):
    _write(tmp_path, {"api.py": "from webcortex import WebCortex\napp = WebCortex('My Shop; touch PWNED')\n"})
    monkeypatch.chdir(tmp_path)
    key = "k$(id)`whoami`'\"x"
    monkeypatch.setenv("WEBCORTEX_API_KEY", key)
    assert cli.main(["mcp-config"]) == 0
    out = capsys.readouterr().out
    config = json.loads(out.partition("\n# Claude Code")[0])
    assert list(config["mcpServers"]) == ["My-Shop-touch-PWNED"]
    line = next(l for l in out.splitlines() if l.startswith("claude mcp add"))
    argv = shlex.split(line)
    assert argv == ["claude", "mcp", "add", "--transport", "http", "My-Shop-touch-PWNED",
                    config["mcpServers"]["My-Shop-touch-PWNED"]["url"], "--header", f"x-api-key: {key}"]

    monkeypatch.delenv("WEBCORTEX_API_KEY")
    assert cli.main(["mcp-config"]) == 0
    line = next(l for l in capsys.readouterr().out.splitlines() if l.startswith("claude mcp add"))
    # Without a key, the shell fills it in when the line is pasted.
    assert line.endswith(' --header "x-api-key: $WEBCORTEX_API_KEY"')


def test_mcp_server_names_are_one_word():
    assert cli.mcp_server_name("demo") == "demo"
    assert cli.mcp_server_name("My Shop") == "My-Shop"
    assert cli.mcp_server_name("a_b-c.d") == "a_b-c-d"
    assert cli.mcp_server_name(";;") == "webcortex" and cli.mcp_server_name(None) == "webcortex"


# ======================================================================
# L29 (CLI half): boot failures are coded errors, not tracebacks
# ======================================================================


def _boot(directory: Path, *args: str, timeout: float = 30) -> subprocess.CompletedProcess:
    env = {k: v for k, v in os.environ.items() if not k.startswith("WEBCORTEX_")}
    env["WEBCORTEX_LOG"] = "error"
    proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", *args], cwd=directory, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        out, err = proc.communicate()
        pytest.fail(f"the server booted instead of refusing:\n{out}\n{err}")
    return subprocess.CompletedProcess(proc.args, proc.returncode, out, err)


PAGE_APP = 'from webcortex import WebCortex\napp = WebCortex("t", templates="templates", port={port})\n' \
           'app.page("/p", "p.html", data={{}})\n'


@pytest.mark.parametrize("command", ["run", "dev"])
def test_a_template_that_does_not_parse_is_wc004_with_file_and_line(tmp_path, command):
    _write(tmp_path, {"api.py": PAGE_APP.format(port=_free_port()), "templates/p.html": "{{ a }}\n{% if %}\n"})
    done = _boot(tmp_path, command)
    assert done.returncode == 1
    assert "Traceback" not in done.stderr
    assert "error[WC004]:" in done.stderr and "(at templates/p.html:2)" in done.stderr


def test_a_missing_template_and_other_boot_failures_are_coded(tmp_path):
    _write(tmp_path, {"api.py": PAGE_APP.format(port=_free_port())})
    (tmp_path / "templates").mkdir()
    done = _boot(tmp_path, "run")
    assert done.returncode == 1 and "Traceback" not in done.stderr
    assert "error[WC004]:" in done.stderr and "(expected at templates/p.html)" in done.stderr

    shutil.rmtree(tmp_path / "templates")
    done = _boot(tmp_path, "run")
    assert "error[WC107]:" in done.stderr and "Traceback" not in done.stderr

    # A port someone else holds.
    with socket.socket() as holder:
        holder.bind(("127.0.0.1", 0))
        holder.listen()
        port = holder.getsockname()[1]
        _write(tmp_path, {"api.py": f"from webcortex import WebCortex\napp = WebCortex('t', port={port})\n"})
        done = _boot(tmp_path, "run")
    assert done.returncode == 1 and "error[WC104]:" in done.stderr and "Traceback" not in done.stderr

    _write(tmp_path, {"api.py": "from webcortex import WebCortex\napp = WebCortex('t', host='999.1.1.1')\n"})
    done = _boot(tmp_path, "run")
    assert done.returncode == 1 and "error[WC101]:" in done.stderr and "bad listen address" in done.stderr


def test_boot_errors_map_to_catalogued_codes():
    assert boot_error_code('template "p.html" failed to load: syntax error') == "WC004"
    assert boot_error_code('invalid application: template "p.html" failed to load: x') == "WC004"
    assert boot_error_code('template directory "t" does not exist') == "WC107"
    assert boot_error_code("bind 127.0.0.1:80 failed: Address already in use (os error 48)") == "WC104"
    assert boot_error_code("database connect failed: (code: 14) unable to open database file") == "WC107"
    assert boot_error_code("invalid manifest: missing field") == "WC101"
    assert boot_error_code("something new", default="WC001") == "WC001"


# ======================================================================
# L42: `webcortex check` prints the route table
# ======================================================================


def test_check_prints_a_route_table(tmp_path, monkeypatch, capsys):
    _write(tmp_path, {"api.py": (
        "from webcortex import WebCortex\n"
        "app = WebCortex('routes', database='sqlite://:memory:')\n"
        "app.api_key('L42_KEY', id='service', scopes=['read', 'write'])\n"
        "app.resource('items', fields={'id': int, 'name': str}, tools=True, read_scopes=['read'],"
        " write_scopes=['write'])\n"
        "@app.get('/hello')\n"
        "def hello() -> dict:\n"
        "    return {}\n")})
    monkeypatch.chdir(tmp_path)
    assert cli.main(["check"]) == 0
    report = json.loads(capsys.readouterr().out)
    table = report["route_table"]
    assert isinstance(report["routes"], int) and report["routes"] == len(table), "the count is unchanged"
    rows = {(r["method"], r["path"]): r for r in table}
    assert rows[("GET", "/items")] == {"method": "GET", "path": "/items", "engine": "query",
                                       "tool": "list_items", "scopes": ["read"]}
    assert rows[("DELETE", "/items/{id}")]["scopes"] == ["write"]
    assert rows[("GET", "/hello")] == {"method": "GET", "path": "/hello", "engine": "python", "tool": None,
                                       "scopes": []}
    assert all(set(r) == {"method", "path", "engine", "tool", "scopes"} for r in table)


# ======================================================================
# halt / release take --url
# ======================================================================

ESTOP_KEY = "estop-admin-key-0123456789abcdef"
ESTOP_APP = """
from webcortex import WebCortex
app = WebCortex("estop", port={port})
app.api_key("ESTOP_KEY", id="admin", scopes=["operate", "webcortex:admin"])

@app.post("/arm", tool=True, tool_name="arm", actuator=True, approval="required", scopes=["operate"])
def arm() -> dict:
    return {{"moved": True}}
"""


@pytest.fixture(scope="module")
def estop_server():
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-estop-"))
    port = _free_port()
    (workdir / "api.py").write_text(ESTOP_APP.format(port=port))
    env = {k: v for k, v in os.environ.items() if not k.startswith("WEBCORTEX_")}
    env.update(ESTOP_KEY=ESTOP_KEY, WEBCORTEX_LOG="warn")
    log = workdir / "server.log"
    with log.open("w") as out:
        proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"], cwd=workdir,
                                stdout=out, stderr=subprocess.STDOUT, env=env)
    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 45
    while time.time() < deadline:
        if proc.poll() is not None:
            pytest.fail(f"server exited early:\n{log.read_text()}")
        try:
            with urllib.request.urlopen(base + "/_webcortex/health", timeout=1):
                break
        except Exception:
            time.sleep(0.1)
    else:
        proc.kill()
        pytest.fail(f"server never became ready:\n{log.read_text()}")
    yield base
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()


def test_halt_and_release_reach_the_app_at_url(estop_server, tmp_path, monkeypatch, capsys):
    _isolate(monkeypatch, "WEBCORTEX_API_KEY", "WEBCORTEX_TOKEN", "WEBCORTEX_HOST", "WEBCORTEX_PORT")
    monkeypatch.chdir(tmp_path)                 # no api.py here: the URL and a key are enough
    assert cli.main(["halt", "--url", estop_server, "--key", ESTOP_KEY, "--reason", "test"]) == 0
    assert capsys.readouterr().out.startswith("HALTED: test.")
    assert cli.main(["halt", "--status", "--url", estop_server + "/", "--key", ESTOP_KEY]) == 0
    assert json.loads(capsys.readouterr().out)["halted"] is True
    assert cli.main(["release", "--url", estop_server, "--key", ESTOP_KEY]) == 0
    assert capsys.readouterr().out.strip() == "released: actuators enabled"

    # An app module here that names another port: --url still wins.
    _write(tmp_path, {"api.py": "from webcortex import WebCortex\napp = WebCortex('estop', port=9)\n"})
    assert cli.main(["release", "--url", estop_server, "--key", ESTOP_KEY]) == 0
    assert capsys.readouterr().out.strip() == "was not halted"
    # The wrong key is the app's own coded refusal.
    assert cli.main(["halt", "--url", estop_server, "--key", "wrong"]) == 1
    assert "error[WC603]" in capsys.readouterr().err


def test_halt_at_a_url_that_is_not_webcortex_is_wc108(tmp_path, monkeypatch, capsys):
    class Page(http.server.BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802
            self.rfile.read(int(self.headers.get("content-length") or 0))
            page = b"<html>not an app</html>"
            self.send_response(200)
            self.send_header("content-type", "text/html")
            self.send_header("content-length", str(len(page)))
            self.end_headers()
            self.wfile.write(page)

        do_POST = do_GET  # noqa: N815

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Page)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    monkeypatch.chdir(tmp_path)
    try:
        url = f"http://127.0.0.1:{server.server_address[1]}"
        assert cli.main(["halt", "--url", url, "--key", "k"]) == 1
        err = capsys.readouterr().err
        assert "error[WC108]" in err and "not with a WebCortex document" in err
        assert cli.main(["release", "--url", "http://[bad", "--key", "k"]) == 1
        assert "error[WC108]" in capsys.readouterr().err
    finally:
        server.shutdown()


# ======================================================================
# K8 (doctor half): a bad OTLP endpoint is reported
# ======================================================================


@pytest.mark.parametrize("value, words", [
    ("otel:4318", "has no http:// or https:// scheme"),
    ("collector.internal:4318/v1/traces", "has no http:// or https:// scheme"),
    ("http://", "names no host"),
    ("http://otel:99999", "does not parse as a URL"),
])
def test_doctor_warns_about_an_otlp_endpoint_that_cannot_work(monkeypatch, value, words):
    _isolate(monkeypatch, "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT")
    monkeypatch.setenv("OTEL_EXPORTER_OTLP_ENDPOINT", value)
    app = WebCortex("k8", port=_free_port())
    check = {c["id"]: c for c in doctor(app)["checks"]}["tracing.export"]
    assert check["status"] == "warn" and words in check["message"]
    assert "OTEL_EXPORTER_OTLP_ENDPOINT" in check["message"] and "will not be exported" in check["message"]


def test_doctor_names_the_otlp_endpoint_it_will_use(monkeypatch):
    _isolate(monkeypatch, "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT")
    app = WebCortex("k8", port=_free_port())
    assert "tracing.export" not in {c["id"] for c in doctor(app)["checks"]}, "nothing configured, nothing said"

    monkeypatch.setenv("OTEL_EXPORTER_OTLP_ENDPOINT", "not-a-url")
    monkeypatch.setenv("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "https://user:SECRET@otel.example/v1/traces")
    check = {c["id"]: c for c in doctor(app)["checks"]}["tracing.export"]
    assert check["status"] == "ok" and "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" in check["message"]
    assert "SECRET" not in check["message"] and "<redacted>@otel.example" in check["message"]

    app.tracing(otlp_endpoint="http://127.0.0.1:4318")   # the declared endpoint wins
    check = {c["id"]: c for c in doctor(app)["checks"]}["tracing.export"]
    assert check["status"] == "ok" and "app.tracing(otlp_endpoint=)" in check["message"]

    app.tracing(enabled=False)
    assert "tracing.export" not in {c["id"] for c in doctor(app)["checks"]}
