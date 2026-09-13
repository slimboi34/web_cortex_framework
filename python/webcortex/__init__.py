"""WebCortex — a Rust-cored Python framework where every route is also an agent tool.

    from webcortex import WebCortex

    app = WebCortex("bookstore", database="sqlite://./app.db")

    app.resource("books", fields={"id": int, "title": str, "year": int}, tools=True)

    @app.get("/books/{id}/blurb", tool=True)
    def blurb(id: int) -> str:
        return f"Book {id} is great."

Then `webcortex dev`. You now have a REST API, an OpenAPI document, and a live MCP
server exposing six tools — from nine lines.
"""

from ._bridge import HTTPError, Request, Response, free_threaded
from .app import WebCortex, Resource

__version__ = "2.0.0"

__all__ = [
    "WebCortex",
    "Request",
    "Response",
    "HTTPError",
    "Resource",
    "free_threaded",
    "__version__",
]
