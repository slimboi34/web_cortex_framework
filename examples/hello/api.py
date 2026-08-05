"""A complete Rango application, exercising every subsystem.

    export RANGO_API_KEY=$(rango keygen)
    rango dev examples/hello/api.py

Then point any MCP client at http://127.0.0.1:8000/_rango/mcp
"""

from dataclasses import dataclass

from rango import HTTPError, Rango

app = Rango(
    "bookstore",
    description="A bookstore that is also an MCP server.",
    database="sqlite://./bookstore.db",
    port=8000,
)

# ---------------------------------------------------------------------------
# 1. Security, declared first because everything below inherits from it.
# ---------------------------------------------------------------------------

app.api_key("RANGO_API_KEY", id="service", scopes=["read", "write", "rango:admin"])
app.rate_limit(per_second=50, burst=100)
app.cors("http://localhost:3000")

# Browsing the catalogue is public; changing it is not.
app.anonymous_scopes("read")


# ---------------------------------------------------------------------------
# 2. A resource: five CRUD routes and five agent tools, all executed in Rust.
# ---------------------------------------------------------------------------

app.resource(
    "books",
    fields={"id": int, "title": str, "author": str, "year": int},
    tools=True,
    read_scopes=["read"],
    write_scopes=["write"],
)


# ---------------------------------------------------------------------------
# 3. A hand-written query. Also never touches Python at request time.
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
    scopes=["read"],
)


# ---------------------------------------------------------------------------
# 4. A Python handler. Parameters are bound by name and typed from the
#    signature, which is also where the agent tool schema comes from.
# ---------------------------------------------------------------------------


@dataclass
class Blurb:
    id: int
    text: str
    words: int


@app.get("/books/{id}/blurb", tool=True, scopes=["read"])
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


@app.get("/slow", scopes=["read"])
async def slow(ms: int = 50) -> dict:
    """An async handler, to exercise the second worker pool."""
    import asyncio
    import threading

    await asyncio.sleep(ms / 1000)
    return {"slept_ms": ms, "thread": threading.current_thread().name}


# ---------------------------------------------------------------------------
# 5. A destructive tool behind a human approval gate.
#
#    An agent that asks for this does not get it: the run suspends and records
#    an approval request. Over MCP it is refused outright, so the gate cannot be
#    stepped around by talking to the tool surface directly.
# ---------------------------------------------------------------------------


@app.delete("/books/all", tool=True, scopes=["write"], approval="required")
def clear_catalogue(confirm: bool = False) -> dict:
    """Delete every book. Requires human approval."""
    if not confirm:
        raise HTTPError(422, "pass confirm=true")
    return {"cleared": True}


# ---------------------------------------------------------------------------
# 6. A gateway route to an external API.
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
    scopes=["read"],
)


# ---------------------------------------------------------------------------
# 7. An agent.
#
#    `scopes` is what a run may do; `expose_scopes` is who may start one. Both
#    are intersected with the caller's own scopes at run time, so the agent can
#    never hold authority its caller lacks. Steps and tokens are capped by the
#    runtime rather than trusted to the model.
# ---------------------------------------------------------------------------

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
    scopes=["read"],
    expose_scopes=["read"],
    max_steps=8,
    token_budget=50_000,
    expose_at="/ask",
)
