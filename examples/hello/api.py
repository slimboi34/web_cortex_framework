"""A complete WebCortex application, exercising every subsystem.

    export WEBCORTEX_API_KEY=$(webcortex keygen)
    webcortex dev examples/hello/api.py

Then point any MCP client at http://127.0.0.1:8000/_webcortex/mcp
"""

from dataclasses import dataclass

from webcortex import HTTPError, WebCortex

app = WebCortex(
    "bookstore",
    description="A bookstore that is also an MCP server.",
    database="sqlite://./bookstore.db",
    port=8000,
)

# ---------------------------------------------------------------------------
# 1. Security, declared first because everything below inherits from it.
# ---------------------------------------------------------------------------

app.api_key("WEBCORTEX_API_KEY", id="service", scopes=["read", "write", "webcortex:admin"])
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
# 7. Models, context and memory.
#
#    Tiers are named once. Point `fast` at a local model to do the cheap work
#    for free: app.models(fast="ollama/qwen3.5:9b").
# ---------------------------------------------------------------------------

app.models(default="claude-opus-5", fast="claude-haiku-4-5-20251001")

app.context(
    "shelf",
    sql="SELECT title, author, year FROM books ORDER BY year DESC LIMIT 25",
    description="The newest books in stock.",
)
app.context("house_style", data={"tone": "warm, specific, never gushing", "max_words": 60})

# Four Rust-executed tools, keyed by whoever is really asking.
app.memory("notes", read_scopes=["read"], write_scopes=["write"])


# ---------------------------------------------------------------------------
# 8. Agents.
#
#    `scopes` is what a run may do; `expose_scopes` is who may start one. Both
#    are intersected with the caller's own scopes at run time, so the agent can
#    never hold authority its caller lacks. Steps and tokens are capped by the
#    runtime rather than trusted to the model, and the librarian's budget is
#    shared by anything it calls.
# ---------------------------------------------------------------------------

app.agent(
    "librarian",
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
    context=["shelf"],
    memory="notes",
    scopes=["read", "write"],
    expose_scopes=["read"],
    max_steps=8,
    token_budget=50_000,
    tool_result_limit=8_000,
    expose_at="/ask",
)

app.agent(
    "copywriter",
    description="Writes a blurb in the house style from whatever it is given.",
    system="Write one blurb. Follow the house style exactly.",
    context=["house_style"],
    scopes=["read"],
    max_steps=2,
    token_budget=10_000,
)

# The front desk hands off rather than answering itself. The conversation and
# the budget carry over; authority can only shrink.
app.agent(
    "front_desk",
    description="First contact. Hands book questions to the librarian.",
    system="Greet briefly. Hand off to the librarian for anything about books.",
    handoffs=["librarian"],
    scopes=["read"],
    expose_scopes=["read"],
    token_budget=60_000,
    context_window=40_000,
    expose_at="/desk",
)


# ---------------------------------------------------------------------------
# 9. A flow: orchestration as data, executed in Rust.
#
#    The librarian researches, the copywriter writes, one shared budget bounds
#    both. Every step is a tool, so this flow is itself a tool an agent could
#    call.
# ---------------------------------------------------------------------------

app.flow(
    "pitch",
    description="Research a request against the catalogue, then write the blurb.",
    pipeline=["librarian", "copywriter"],
    scopes=["read"],
    token_budget=80_000,
)


# ---------------------------------------------------------------------------
# 10. A behaviour with concurrent leaves.
#
#     The loop is Python; the fifty classifications happen in one wait on the
#     fast tier.
# ---------------------------------------------------------------------------


@app.behaviour(
    "shelve",
    description="Tag every book with a genre, concurrently, on the fast model.",
    tools=["list_books", "update_books"],
    context=["house_style"],
    scopes=["read", "write"],
    max_steps=200,
    token_budget=100_000,
    model="fast",
)
def shelve(ctx, input):
    books = ctx.call("list_books", limit=50)
    if not books:
        ctx.halt("nothing to shelve")
    genres = ctx.ask_many(
        [f"Genre for '{b['title']}' by {b['author']} ({b['year']})?" for b in books],
        schema={
            "type": "object",
            "properties": {"genre": {"enum": ["fiction", "non-fiction", "poetry", "reference"]}},
            "required": ["genre"],
        },
    )
    ctx.gather(*[
        ("update_books", {"id": b["id"], "title": b["title"], "author": b["author"],
                          "year": b["year"]})
        for b in books
    ])
    return {"shelved": len(books), "genres": [g["genre"] for g in genres], "usage": ctx.usage}

