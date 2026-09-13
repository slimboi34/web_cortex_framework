# AI-native development

Django and Rails were designed to be written by people. They are batteries
included because a person has to assemble the batteries. WebCortex is designed
to be written *with* a model, which changes what the framework should ship:
fewer batteries, and a shape a model can hold in its head.

Three things make that real.

---

## The context pack

An AI coding tool working on your app should not need to read the framework's
source, this site and every file in the project. It needs the app's **shape**
— what exists, what it is called, what it accepts, who may call it — and the
handful of framework signatures that matter.

```console
$ webcortex context
```

prints exactly that, derived from the manifest so it cannot drift from the
code, in a few thousand tokens:

```markdown
# supportdesk — WebCortex context pack
version 0.1.0; database sqlite://./supportdesk.db; control plane at /_webcortex.

## Security
- auth: service (env WEBCORTEX_API_KEY, scopes ['read', 'write', 'webcortex:admin']); anonymous scopes ['read']; rate-limited; no cors
- public routes (7): GET /, GET /tickets, GET /tickets/{id}, …

## Routes (19, 17 served in Rust)
| Method | Path | Engine | Tool | Scopes |
|---|---|---|---|---|
| GET | /tickets | query | list_tickets | read |
…

## Tools (14)
- `list_tickets(limit?: integer, offset?: integer)` — Return a page of tickets rows…
- `triage(threshold?: integer)` — Classify every open ticket at once…

## Agents (3)
- **front_desk** — First contact. Routes to a specialist or answers directly.
  model=default; tools=[…]; handoffs=['billing', 'technical']; context=['policy']; …

## WebCortex 2 — how to extend this app
app.resource("things", fields={...}, tools=True, read_scopes=[...], write_scopes=[...])
…
```

Paste it into a conversation, keep it in a `CLAUDE.md`, or let
`webcortex evolve` use it. Because it is small, it can be re-sent on every
turn of a long session without the session becoming expensive.

---

## `webcortex evolve`

Hit the app with a prompt:

```console
$ webcortex evolve "add a reviews resource tied to books, and a behaviour that
  summarises the reviews for a book with the fast model" --model fast
```

The command builds the context pack, adds the request, and asks the model —
resolved with the app's own aliases and providers, so `--model fast` means
whatever `fast` means in `api.py`, including a local Ollama model — for
Python to append to `api.py`. The system prompt tells it to prefer
declarations over handlers, to scope everything, to gate destructive tools,
and to use the fast tier for classification leaves.

It **prints a proposal**; it does not edit your file. `--out evolve.py`
writes it somewhere to review, and `--json` asks for `{summary, code, notes}`
instead of bare code. Then:

```console
$ webcortex check       # a typo in a tool name is a boot error with a hint
$ webcortex security    # what the new routes expose
```

The loop is: describe → propose → check → run. The framework's boot-time
validation is what makes the loop safe to repeat: a wrong tool name, an
undeclared context, a gate on a non-tool, a duplicate route — all fail at
`check`, with a message a model can act on, before anything runs.

---

## `AGENTS.md`

The repository ships an [`AGENTS.md`](https://github.com/slimboi34/web_cortex_framework/blob/main/AGENTS.md):
the complete API with exact signatures and defaults, the binding and scope
rules, the constraints the runtime enforces, and the mistakes that are cheap
to make and expensive to debug. Claude Code, Cursor, Codex, Aider and Copilot
Workspace read it by convention. It is written to be correct rather than
welcoming; humans should start with this site.

---

## Why this shape

A framework is easy for a model to write against when:

- **Everything is a declaration with a stable signature.** `app.resource`,
  `app.query`, `app.context`, `app.flow`, `app.memory` are calls with keyword
  arguments, not class hierarchies to subclass or files to arrange. A model
  can emit them from a one-line description.
- **The docstring is the tool description and the signature is the schema.**
  There is no second artifact to keep consistent, so a model cannot get it
  wrong by forgetting one.
- **Mistakes fail at boot, with a hint.** `webcortex check` is a fast,
  deterministic oracle a model can run after every edit.
- **The runtime holds the safety, not the prompt.** Scopes intersect, budgets
  compose, gates hold on every path. Generated code that forgets a guard is
  still bounded by the guards that cannot be forgotten.
- **The app describes itself.** The context pack, OpenAPI, the MCP tool
  list and `/_webcortex/*` are projections of one manifest.

The consequence is a development loop with a different shape from Django's:
not "read the docs, scaffold, configure, wire, test", but "describe, check,
run, describe the next thing". Human judgement goes where it is needed —
what to build, what to gate, what to spend — and the assembly is delegated.

---

## Keeping the framework itself improving

The repository also ships [`scout/`](https://github.com/slimboi34/web_cortex_framework/tree/main/scout):
a small, dependency-free reviewer that runs a local model over recent changes
and a rotating focus area, and appends structured suggestions to a Markdown
file for a human — or a coding agent — to pick up. It is deliberately not
part of the framework; it is the framework's own development loop, written
down.

[Security :material-arrow-right:](security.md){ .md-button .md-button--primary }
