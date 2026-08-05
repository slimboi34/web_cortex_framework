"""Security declarations and their enforcement.

These are the tests worth being pedantic about: every one of them corresponds to
a way a real deployment gets breached.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from webcortex import WebCortex

# ---------------------------------------------------------------- declarations


def app_with_auth() -> WebCortex:
    app = WebCortex("sec", database="sqlite://:memory:")
    app.api_key("ADMIN_KEY", id="admin", scopes=["read", "write", "webcortex:admin"])
    app.api_key("READER_KEY", id="reader", scopes=["read"])
    return app


def test_api_keys_are_referenced_by_env_var_never_embedded():
    app = app_with_auth()
    manifest = app.manifest()
    serialized = json.dumps(manifest)
    assert "ADMIN_KEY" in manifest["auth"]["api_keys"]
    # The manifest is logged, diffed, and handed to agents. It must be safe.
    assert "secret" not in serialized.lower() or "WEBCORTEX" in serialized


def test_resource_splits_read_and_write_authorization():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource(
        "items",
        fields={"id": int, "name": str},
        read_scopes=["read"],
        write_scopes=["write"],
    )
    routes = {(r["method"], r["path"]): r["scopes"] for r in app.manifest()["routes"]}
    assert routes[("GET", "/items")] == ["read"]
    assert routes[("GET", "/items/{id}")] == ["read"]
    assert routes[("POST", "/items")] == ["write"]
    assert routes[("PUT", "/items/{id}")] == ["write"]
    assert routes[("DELETE", "/items/{id}")] == ["write"]


def test_resource_scopes_shorthand_applies_to_everything():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("items", fields={"id": int}, scopes=["all"])
    for r in app.manifest()["routes"]:
        if r["path"].startswith("/items"):
            assert r["scopes"] == ["all"]


def test_a_resource_with_no_scopes_is_reported_as_public():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("items", fields={"id": int, "name": str})
    report = app.security_report()
    assert "POST /items" in report["public_routes"]
    assert report["auth_configured"] is False


def test_security_report_lists_gated_tools():
    app = WebCortex("t")

    @app.post("/danger", tool=True, approval="required")
    def danger() -> dict:
        return {}

    assert app.security_report()["gated_tools"]


def test_cors_wildcard_with_credentials_is_rejected_at_boot():
    app = WebCortex("t")
    app.cors("*", credentials=True)
    with pytest.raises(ValueError, match="allow_credentials"):
        app.check()


def test_cors_with_explicit_origins_and_credentials_is_fine():
    app = WebCortex("t")
    app.cors("https://app.test", credentials=True)
    app.check()


def test_cors_enabled_without_origins_is_rejected():
    app = WebCortex("t")
    app.cors()
    with pytest.raises(ValueError, match="allow_origins"):
        app.check()


def test_approval_gate_on_a_non_tool_route_is_rejected():
    """A gate that can never fire is dead config giving false confidence."""
    app = WebCortex("t")

    @app.post("/danger", approval="required")
    def danger() -> dict:
        return {}

    with pytest.raises(ValueError, match="not exposed as a tool"):
        app.check()


def test_agent_tool_typo_suggests_the_intended_name():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("items", fields={"id": int}, tools=True)
    app.agent("a", model="m", tools=["list_item"])  # missing the 's'
    with pytest.raises(ValueError, match="Did you mean"):
        app.check()


def test_duplicate_agent_names_are_rejected():
    app = WebCortex("t")
    app.agent("a", model="m")
    app.agent("a", model="m")
    with pytest.raises(ValueError, match="duplicate agent"):
        app.check()


def test_an_agent_that_could_never_act_is_rejected():
    app = WebCortex("t")
    app.agent("a", model="m", max_steps=0)
    with pytest.raises(ValueError, match="never act"):
        app.check()


def test_security_headers_are_on_by_default():
    assert WebCortex("t").manifest()["security_headers"]["enabled"] is True


def test_rate_limit_is_off_until_declared():
    assert WebCortex("t").manifest()["rate_limit"]["enabled"] is False
    app = WebCortex("t")
    app.rate_limit(10, burst=20)
    assert app.manifest()["rate_limit"] == {
        "enabled": True, "per_second": 10, "burst": 20, "idle_eviction_secs": 300,
    }


def test_page_without_templates_configured_is_rejected():
    app = WebCortex("t")
    app.page("/x", "x.html", data={"a": 1})
    with pytest.raises(ValueError, match="template"):
        app.check()


def test_page_cannot_take_two_data_sources():
    app = WebCortex("t", database="sqlite://:memory:", templates="templates")
    with pytest.raises(ValueError, match="not both"):
        app.page("/x", "x.html", sql="SELECT 1", data={"a": 1})


def test_static_files_requires_a_real_directory():
    app = WebCortex("t")
    with pytest.raises(ValueError, match="not a directory"):
        app.static_files("/assets", "/nonexistent/path/xyz")


def test_default_root_route_yields_to_a_user_defined_one(tmp_path):
    """The framework must never claim a path the user cannot take back."""
    (tmp_path / "home.html").write_text("<h1>mine</h1>")
    app = WebCortex("t", templates=str(tmp_path))
    app.page("/", "home.html")
    roots = [r for r in app.manifest()["routes"] if r["path"] == "/"]
    assert len(roots) == 1
    assert roots[0]["op"]["kind"] == "page"


def test_default_root_route_appears_when_unused():
    app = WebCortex("t")
    roots = [r for r in app.manifest()["routes"] if r["path"] == "/"]
    assert len(roots) == 1
    assert roots[0]["op"]["kind"] == "static"


# ---------------------------------------------------------------- live server


APP = '''
from webcortex import WebCortex

app = WebCortex("sec", database="sqlite://./sec.db", port={port})
app.api_key("ADMIN_KEY", id="admin", scopes=["read", "write", "webcortex:admin"])
app.api_key("READER_KEY", id="reader", scopes=["read", "webcortex:admin"])
app.anonymous_scopes()
app.cors("https://allowed.test")

app.resource(
    "items", fields={{"id": int, "name": str}}, tools=True,
    read_scopes=["read"], write_scopes=["write"],
)
app.static("GET", "/open", {{"public": True}})
'''


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="module")
def secure_server():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-sec-"))
    (workdir / "api.py").write_text(APP.format(port=port))
    log_path = workdir / "server.log"

    env = {
        **os.environ,
        "WEBCORTEX_LOG": "warn",
        "ADMIN_KEY": "admin-secret-key-1234567890",
        "READER_KEY": "reader-secret-key-1234567890",
    }
    with log_path.open("w") as log:
        proc = subprocess.Popen(
            [sys.executable, "-m", "webcortex.cli", "run", "api.py"],
            cwd=workdir, stdout=log, stderr=subprocess.STDOUT, env=env,
        )

    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 45
    while time.time() < deadline:
        if proc.poll() is not None:
            pytest.fail(f"server exited early:\n{log_path.read_text()}")
        try:
            with urllib.request.urlopen(base + "/_webcortex/health", timeout=1):
                break
        except Exception:
            time.sleep(0.1)
    else:
        proc.kill()
        pytest.fail(f"server never became ready:\n{log_path.read_text()}")

    yield base
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def call(base, path, method="GET", key=None, body=None, headers=None):
    data = json.dumps(body).encode() if body is not None else None
    h = {"content-type": "application/json", **(headers or {})}
    if key:
        h["x-api-key"] = key
    req = urllib.request.Request(base + path, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else None, dict(r.headers)
    except urllib.error.HTTPError as e:
        raw = e.read()
        return e.code, (json.loads(raw) if raw else None), dict(e.headers)


ADMIN = "admin-secret-key-1234567890"
READER = "reader-secret-key-1234567890"


def test_anonymous_is_refused_on_a_scoped_route(secure_server):
    assert call(secure_server, "/items")[0] == 401


def test_an_invalid_key_is_401_not_a_silent_downgrade(secure_server):
    status, body, _ = call(secure_server, "/items", key="not-a-real-key")
    assert status == 401
    assert "API key" in json.dumps(body)


def test_reader_can_read_but_not_write(secure_server):
    assert call(secure_server, "/items", key=READER)[0] == 200
    status, body, _ = call(
        secure_server, "/items", "POST", key=READER, body={"name": "x"}
    )
    assert status == 403, "authenticated but unauthorized must be 403, not 401"
    assert "write" in json.dumps(body)


def test_admin_can_write(secure_server):
    status, created, _ = call(
        secure_server, "/items", "POST", key=ADMIN, body={"name": "widget"}
    )
    assert status == 200 and created["name"] == "widget"


def test_unscoped_route_stays_public(secure_server):
    assert call(secure_server, "/open")[0] == 200


def test_security_headers_on_every_response(secure_server):
    for path, key in [("/open", None), ("/items", None)]:
        _, _, headers = call(secure_server, path, key=key)
        assert headers.get("x-content-type-options") == "nosniff"
        assert headers.get("x-frame-options") == "DENY"
        assert "x-request-id" in headers


def test_request_id_is_echoed_when_supplied(secure_server):
    _, _, headers = call(
        secure_server, "/open", headers={"x-request-id": "trace-me-123"}
    )
    assert headers.get("x-request-id") == "trace-me-123"


def test_cors_allows_the_declared_origin_only(secure_server):
    _, _, allowed = call(secure_server, "/open", headers={"origin": "https://allowed.test"})
    assert allowed.get("access-control-allow-origin") == "https://allowed.test"
    assert allowed.get("vary") == "Origin"

    _, _, denied = call(secure_server, "/open", headers={"origin": "https://evil.test"})
    assert "access-control-allow-origin" not in denied


def test_control_plane_requires_admin_scope(secure_server):
    assert call(secure_server, "/_webcortex/openapi.json")[0] == 401
    assert call(secure_server, "/_webcortex/openapi.json", key=ADMIN)[0] == 200


def test_health_stays_reachable_without_a_credential(secure_server):
    """Load balancers cannot present an API key."""
    assert call(secure_server, "/_webcortex/health")[0] == 200


def test_security_endpoint_reports_the_public_surface(secure_server):
    status, body, _ = call(secure_server, "/_webcortex/security", key=ADMIN)
    assert status == 200
    assert body["auth_configured"] is True
    assert "GET /open" in body["public_routes"]
    assert "GET /items" not in body["public_routes"]
