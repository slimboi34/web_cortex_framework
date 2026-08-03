"""A small but complete Pylon application.

Demonstrates all five route kinds and shows which of them the interpreter is
involved in. Run it with:

    pylon dev examples/hello/api.py

Then point any MCP client at http://127.0.0.1:8000/_pylon/mcp
"""

from dataclasses import dataclass

from pylon import HTTPError, Pylon

app = Pylon(
    "bookstore",
    description="A tiny bookstore that is also an MCP server.",
    database="sqlite://./bookstore.db",
    port=8000,
)

# ---------------------------------------------------------------------------
# 1. A resource: five CRUD routes and five agent tools, all executed in Rust.
# ---------------------------------------------------------------------------

app.resource(
    "books",
    fields={"id": int, "title": str, "author": str, "year": int},
    tools=True,
)


# ---------------------------------------------------------------------------
# 2. A hand-written query. Still never touches Python at request time.
# ---------------------------------------------------------------------------

app.query(
    "GET",
    "/books/by-author/{author}",
    "SELECT * FROM books WHERE author = ? ORDER BY year DESC",
    params=["author"],
    returns="many",
    summary="List an author's books, newest first",
    description="Every book by the given author, most recent publication first.",
    input_schema={
        "type": "object",
        "properties": {"author": {"type": "string"}},
        "required": ["author"],
    },
    tool=True,
    tool_name="books_by_author",
)


# ---------------------------------------------------------------------------
# 3. A Python handler. Parameters are bound by name and typed from the
#    signature, which is also where the tool schema comes from.
# ---------------------------------------------------------------------------


@dataclass
class Blurb:
    id: int
    text: str
    words: int


@app.get("/books/{id}/blurb", tool=True)
def blurb(id: int, style: str = "plain") -> Blurb:
    """Generate a short pitch for a book.

    `style` may be "plain" or "loud".
    """
    if style not in ("plain", "loud"):
        raise HTTPError(422, f"unknown style {style!r}; use 'plain' or 'loud'")
    text = f"Book {id} is worth your evening."
    if style == "loud":
        text = text.upper() + "!!!"
    return Blurb(id=id, text=text, words=len(text.split()))


# ---------------------------------------------------------------------------
# 4. An async handler, to show both pools in use.
# ---------------------------------------------------------------------------


@app.get("/slow", tool=False)
async def slow(ms: int = 50) -> dict:
    """Sleep, then report which OS thread served the request."""
    import asyncio
    import threading

    await asyncio.sleep(ms / 1000)
    return {"slept_ms": ms, "thread": threading.current_thread().name}


# ---------------------------------------------------------------------------
# 5. A gateway route to an external API, and an agent that can use everything
#    above as tools.
# ---------------------------------------------------------------------------

app.upstream("openlibrary", base_url="https://openlibrary.org", timeout_ms=8000)

app.proxy(
    "GET",
    "/external/search",
    upstream="openlibrary",
    rewrite="/search.json",
    summary="Search Open Library",
    description="Proxy to Open Library's search API. Pass ?q=<terms>.",
    tool=True,
    tool_name="search_open_library",
)

app.agent(
    "librarian",
    model="claude-opus-5",
    description="Answers questions about the catalogue.",
    system=(
        "You are a librarian for this bookstore. Prefer the local catalogue; "
        "fall back to Open Library only when a book is not stocked."
    ),
    tools=[
        "list_books",
        "get_books",
        "books_by_author",
        "get_books_by_id_blurb",
        "search_open_library",
    ],
    expose_at="/ask",
)
