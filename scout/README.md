# The scout

A local model that reads this repository on a schedule and leaves suggestions
for its next iteration. It is the framework's own development loop, written
down: **the scout proposes, a person or a coding agent decides.**

```
scout/
  scout.py             the reviewer (standard library only)
  install_launchd.sh   run it every six hours on macOS
  suggestions.md       what it found — git-ignored, read it locally
  state.json           last commit reviewed, rotation position, de-dup set
  logs/                stdout/stderr from scheduled runs
```

## How it works

Each run is a deterministic loop with probabilistic leaves — the same shape as
a Behaviour:

1. **Is Ollama up, with the model pulled?** If not, exit quietly. The scout
   never talks to anything but the local model.
2. **What to read.** Files changed since the last reviewed commit, then the
   next *focus area* in a fixed rotation (agent runtime → providers → flows →
   dispatcher → security → manifest → bridge → Python API → tooling →
   starters → tests → docs → design), up to `SCOUT_MAX_FILES` per run. Over a
   week the whole repository is revisited.
3. **Ask.** One model call per file chunk, with the answer forced into a JSON
   schema: `title`, `area`, `severity`, `where`, `rationale`, `proposal`. The
   system prompt states the framework's goals and what is deliberately out of
   scope, so the model does not propose an ORM.
4. **Write.** De-duplicate by file and title against everything already
   suggested, then append a dated, commit-tagged section to
   `suggestions.md`.

## Run it

```bash
ollama pull qwen3.5:9b                  # once
python3 scout/scout.py                  # one pass, a few minutes on an M-series machine
./scout/install_launchd.sh              # every six hours, and on login
./scout/install_launchd.sh --uninstall
```

| Variable | Default | |
|---|---|---|
| `SCOUT_MODEL` | `qwen3.5:9b` | Any Ollama model that supports structured output |
| `OLLAMA_HOST` | `http://127.0.0.1:11434` | |
| `SCOUT_MAX_FILES` | `8` | Files per run |
| `SCOUT_INTERVAL_SECS` | `21600` | launchd interval (installer only) |

## Pick up the suggestions

Open `scout/suggestions.md`. Each entry ends with `**Status:** open`. Implement
it and mark it `done`, or mark it `rejected` with a word on why. An AI coding
tool can be told:

> Read `scout/suggestions.md`, implement the open suggestions you agree with,
> mark each one `done` or `rejected` with a reason, and run the tests.

Rejected ideas do not come back unless the file changes.

## What it is not

Not part of the `webcortex` package, not imported by it, not run in CI. A 9B
model reviewing a file is a source of *leads*, not of decisions: some will be
sharp, some will be wrong, and the point of the fixed schema and the human
step is that it costs a minute to tell which.
