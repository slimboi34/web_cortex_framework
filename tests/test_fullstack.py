"""Server-rendered pages, static assets, TypeScript generation, and starters."""

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
from webcortex import starters


# ---------------------------------------------------------------- starters


@pytest.mark.parametrize("template", ["api", "fullstack", "agent"])
def test_every_starter_produces_a_valid_app(template, tmp_path, monkeypatch):
    files = starters.files_for(template, "demo", "a demo")
    for relative, contents in files.items():
        path = tmp_path / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)

    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("WEBCORTEX_API_KEY", "test-key-abcdefghijklmnop")

    from webcortex.cli import _load_app

    app = _load_app("api.py")
    report = app.check()  # raises if the manifest is invalid
    assert report["routes"] > 0
    assert report["tools"], "a starter should demonstrate the tool surface"


@pytest.mark.parametrize("template", ["api", "fullstack", "agent"])
def test_every_starter_is_secure_by_default(template, tmp_path, monkeypatch):
    """A starter that ships an insecure app teaches an insecure habit."""
    files = starters.files_for(template, "demo", "a demo")
    for relative, contents in files.items():
        path = tmp_path / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)

    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("WEBCORTEX_API_KEY", "test-key-abcdefghijklmnop")

    from webcortex.cli import _load_app

    report = _load_app("api.py").security_report()
    assert report["auth_configured"] is True
    assert report["rate_limited"] is True
    assert report["security_headers"] is True

    # Mutating routes must never be reachable without a credential.
    writes = [r for r in report["public_routes"] if r.split()[0] in ("POST", "PUT", "DELETE")]
    assert writes == [], f"{template} starter leaves writes public: {writes}"


def test_unknown_starter_is_rejected():
    with pytest.raises(ValueError, match="unknown starter"):
        starters.files_for("nope", "x", "y")


# ---------------------------------------------------------------- typegen


def test_typescript_client_covers_every_api_route():
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("books", fields={"id": int, "title": str}, tools=True)

    ts = app.typescript_client()
    for method in ("listBooks", "getBooks", "createBooks", "updateBooks", "deleteBooks"):
        assert f"{method}(" in ts, f"missing {method}:\n{ts[:500]}"
    assert "export class WebCortexClient" in ts
    assert "x-api-key" in ts, "the client must be able to authenticate"


def test_typescript_client_omits_pages_and_static(tmp_path):
    (tmp_path / "home.html").write_text("<h1>hi</h1>")
    (tmp_path / "assets").mkdir()
    app = WebCortex("t", templates=str(tmp_path))
    app.page("/home", "home.html")
    app.static_files("/assets", str(tmp_path / "assets"))
    app.static("GET", "/api/thing", {"ok": True})

    ts = app.typescript_client()
    assert "/api/thing" in ts
    assert "/home" not in ts
    assert "/assets" not in ts


def test_typescript_signatures_never_wrap():
    """Multi-line types spliced into a signature produce invalid TypeScript."""
    app = WebCortex("t", database="sqlite://:memory:")
    app.resource("books", fields={"id": int, "title": str, "author": str})
    ts = app.typescript_client()
    for line in ts.splitlines():
        if "): Promise<" in line:
            assert line.count("<") == line.count(">"), f"unbalanced generics: {line}"
            assert line.rstrip().endswith("{"), f"signature wrapped: {line}"
    assert ts.count("{") == ts.count("}"), "unbalanced braces"


# ---------------------------------------------------------------- live pages


APP = '''
from webcortex import WebCortex

app = WebCortex("fs", database="sqlite://./fs.db", templates="templates", port={port})

app.resource("items", fields={{"id": int, "name": str}}, tools=True)

app.page("/", "index.html", sql="SELECT * FROM items ORDER BY id", bind="items")
app.page("/about", "about.html", data={{"title": "About Us"}})


@app.page_handler("/dash", "dash.html")
def dash() -> dict:
    return {{"count": 42}}


app.static_files("/assets", "static")
'''

INDEX = "{% for i in items %}<li>{{ i.name }}</li>{% endfor %}"
ABOUT = "<h1>{{ data.title }}</h1>"
DASH = "<p>{{ data.count }}</p><span>{{ user.authenticated }}</span>"
XSS = "{% for i in items %}<li>{{ i.name }}</li>{% endfor %}"


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="module")
def page_server():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-fs-"))
    (workdir / "templates").mkdir()
    (workdir / "static").mkdir()
    (workdir / "api.py").write_text(APP.format(port=port))
    (workdir / "templates/index.html").write_text(INDEX)
    (workdir / "templates/about.html").write_text(ABOUT)
    (workdir / "templates/dash.html").write_text(DASH)
    (workdir / "static/app.css").write_text("body{color:red}")
    (workdir / "static/.secret").write_text("do not serve me")
    log_path = workdir / "server.log"

    env = {**os.environ, "WEBCORTEX_LOG": "warn"}
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


def fetch(base, path, headers=None):
    req = urllib.request.Request(base + path, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, r.read().decode(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)


def post_item(base, name):
    req = urllib.request.Request(
        base + "/items",
        data=json.dumps({"name": name}).encode(),
        method="POST",
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.loads(r.read())


def test_page_renders_data_from_sql_without_python(page_server):
    post_item(page_server, "Widget")
    status, body, headers = fetch(page_server, "/")
    assert status == 200
    assert "Widget" in body
    assert headers["content-type"].startswith("text/html")


def test_page_renders_constant_data(page_server):
    status, body, _ = fetch(page_server, "/about")
    assert (status, "About Us" in body) == (200, True)


def test_page_handler_supplies_context_from_python(page_server):
    status, body, _ = fetch(page_server, "/dash")
    assert status == 200
    assert "42" in body


def test_pages_receive_the_authenticated_user(page_server):
    _, body, _ = fetch(page_server, "/dash")
    assert "false" in body.lower(), "anonymous caller should not read as authenticated"


def test_template_output_is_html_escaped(page_server):
    post_item(page_server, "<script>alert(1)</script>")
    _, body, _ = fetch(page_server, "/")
    assert "<script>alert(1)</script>" not in body
    assert "&lt;script&gt;" in body


def test_static_files_are_served_with_an_etag(page_server):
    status, body, headers = fetch(page_server, "/assets/app.css")
    assert status == 200
    assert "color:red" in body
    assert headers["content-type"].startswith("text/css")
    etag = headers["etag"]

    status, _, _ = fetch(page_server, "/assets/app.css", {"if-none-match": etag})
    assert status == 304


def test_static_dotfiles_are_never_served(page_server):
    assert fetch(page_server, "/assets/.secret")[0] == 404


def test_static_traversal_is_blocked(page_server):
    for attack in ["/assets/../api.py", "/assets/..%2fapi.py", "/assets/%2e%2e/api.py"]:
        assert fetch(page_server, attack)[0] == 404, f"leaked via {attack}"


def test_pages_are_absent_from_the_openapi_document(page_server):
    _, body, _ = fetch(page_server, "/_webcortex/openapi.json")
    spec = json.loads(body)
    assert "/about" not in spec["paths"]
    assert "/items" in spec["paths"]
