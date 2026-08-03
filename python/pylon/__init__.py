"""Pylon — a Rust-cored Python framework where every route is also an agent tool.

    from pylon import Pylon

    app = Pylon("bookstore", database="sqlite://./app.db")

    app.resource("books", fields={"id": int, "title": str, "year": int}, tools=True)

    @app.get("/books/{id}/blurb", tool=True)
    def blurb(id: int) -> str:
        return f"Book {id} is great."

Then `pylon dev`. You now have a REST API, an OpenAPI document, and a live MCP
server exposing six tools — from nine lines.
"""

from ._bridge import HTTPError, Request, Response, free_threaded
from .app import Pylon, Resource

__version__ = "0.1.0"

__all__ = [
    "Pylon",
    "Request",
    "Response",
    "HTTPError",
    "Resource",
    "free_threaded",
    "__version__",
]
