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
