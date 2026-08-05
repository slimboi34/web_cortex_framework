"""Manifest construction and boot-time validation.

The framework's promise is that a misconfigured app fails at boot with a clear
message rather than at 3am with a 500, so most of these assert on errors.
"""

from __future__ import annotations

import pytest

from webcortex import WebCortex


def make_app(**kw) -> WebCortex:
    return WebCortex("t", database="sqlite://:memory:", **kw)


def test_resource_generates_five_routes_and_five_tools():
    app = make_app()
    app.resource("books", fields={"id": int, "title": str}, tools=True)
    report = app.check()

    assert set(report["tools"]) >= {
        "list_books", "get_books", "create_books", "update_books", "delete_books",
    }
    # The root route is static; the five CRUD routes are queries. None are Python.
    assert report["native_routes"] == report["routes"]


def test_resource_routes_are_all_declarative():
    app = make_app()
    app.resource("books", fields={"id": int, "title": str})
    ops = {r["op"]["kind"] for r in app.manifest()["routes"] if r["path"].startswith("/books")}
    assert ops == {"query"}, "CRUD must never compile to a Python op"


def test_python_handler_schema_comes_from_the_signature():
    app = make_app()

    @app.get("/books/{id}/blurb", tool=True)
    def blurb(id: int, style: str = "plain") -> str:
        """Pitch a book."""
        return ""

    route = next(r for r in app.manifest()["routes"] if r["path"].endswith("blurb"))
    assert route["op"]["kind"] == "python"
    assert route["input_schema"]["properties"]["id"] == {"type": "integer"}
    assert route["input_schema"]["required"] == ["id"]
    assert route["summary"] == "Pitch a book."
    assert route["tool"]["read_only"] is True, "GET should default to read-only"


def test_tool_name_is_derived_when_not_given():
    app = make_app()

    @app.get("/books/{id}/blurb", tool=True)
    def blurb(id: int) -> str:
        return ""

    assert "get_books_by_id_blurb" in app.check()["tools"]


def test_write_methods_are_not_read_only_by_default():
    app = make_app()

    @app.post("/things")
    def create() -> dict:
        return {}

    route = next(r for r in app.manifest()["routes"] if r["path"] == "/things")
    assert route["tool"]["read_only"] is False


def test_duplicate_route_is_a_boot_error():
    app = make_app()
    app.static("GET", "/dup", {})
    app.static("GET", "/dup", {})
    with pytest.raises(ValueError, match="duplicate route"):
        app.check()


def test_proxy_to_undeclared_upstream_is_a_boot_error():
    app = make_app()
    app.proxy("GET", "/x", upstream="ghost")
    with pytest.raises(ValueError, match="ghost"):
        app.check()


def test_agent_referencing_a_nonexistent_tool_is_a_boot_error():
    app = make_app()
    app.agent("a", model="m", tools=["not_a_tool"])
    with pytest.raises(ValueError, match="not_a_tool"):
        app.check()


def test_agent_referencing_a_real_tool_validates():
    app = make_app()
    app.resource("books", fields={"id": int, "title": str}, tools=True)
    app.agent("librarian", model="m", tools=["list_books"])
    assert app.check()["agents"] == ["librarian"]


def test_two_routes_cannot_claim_the_same_tool_name():
    app = make_app()
    app.static("GET", "/a", {}, tool=True, tool_name="same")
    app.static("GET", "/b", {}, tool=True, tool_name="same")
    # Duplicate tool names are caught when the app is built, not when a model
    # calls the ambiguous name.
    with pytest.raises(Exception):
        app.check()


def test_query_route_without_a_database_is_rejected():
    app = WebCortex("t")  # no database
    with pytest.raises(ValueError, match="database"):
        app.query("GET", "/x", "SELECT 1")


def test_resource_without_a_database_is_rejected():
    app = WebCortex("t")
    with pytest.raises(ValueError, match="database"):
        app.resource("books", fields={"id": int})


def test_primary_key_must_be_a_declared_field():
    app = make_app()
    with pytest.raises(ValueError, match="primary key"):
        app.resource("books", fields={"title": str}, primary_key="id")


def test_bad_method_and_path_are_rejected():
    app = make_app()
    with pytest.raises(ValueError, match="unsupported HTTP method"):
        app.static("FETCH", "/x", {})
    with pytest.raises(ValueError, match="must start with"):
        app.static("GET", "x", {})


def test_upstream_credentials_are_referenced_not_embedded():
    app = make_app()
    app.upstream("api", base_url="https://x.test", bearer_env="API_TOKEN")
    up = app.manifest()["upstreams"]["api"]
    assert up["bearer_env"] == "API_TOKEN"
    assert "API_TOKEN" not in str(up.get("headers", {}))


def test_openapi_documents_which_engine_serves_each_route():
    app = make_app()
    app.resource("books", fields={"id": int, "title": str}, tools=True)

    @app.get("/custom")
    def custom() -> dict:
        return {}

    spec = app.openapi()
    assert spec["openapi"] == "3.1.0"
    assert spec["paths"]["/books"]["get"]["x-webcortex-op"] == "query"
    assert spec["paths"]["/custom"]["get"]["x-webcortex-op"] == "python"
    assert spec["x-webcortex"]["mcp_endpoint"] == "/_webcortex/mcp"


def test_openapi_strips_path_params_from_the_request_body():
    app = make_app()
    app.resource("books", fields={"id": int, "title": str})
    put = app.openapi()["paths"]["/books/{id}"]["put"]
    body_props = put["requestBody"]["content"]["application/json"]["schema"]["properties"]
    assert "id" not in body_props, "the id lives in the URL, not the body"
    assert "title" in body_props


def test_generated_ddl_marks_the_primary_key():
    app = make_app()
    app.resource("books", fields={"id": int, "title": str, "year": int})
    sql = app.schema_sql
    assert "CREATE TABLE IF NOT EXISTS books" in sql
    assert "id INTEGER PRIMARY KEY" in sql
    assert "title TEXT" in sql
