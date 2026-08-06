# Installation

## Requirements

| | |
|---|---|
| Python | 3.12, 3.13 or 3.14. Free-threaded 3.14 (`3.14t`) is the fast path — see below. |
| OS | Linux (x86_64/aarch64), macOS (Apple Silicon/Intel), Windows (x64) |
| Rust | Only if building from source, or installing from an sdist |

## From PyPI

```console
$ pip install web-cortex-framework
```

!!! warning "On CPython 3.14, use 0.3.2 or later"
    0.3.1 and earlier fail to import on CPython 3.14.7 with
    `ValueError: module functions cannot set METH_CLASS or METH_STATIC`. This was
    a pyo3 bug, fixed in 0.3.2. Nothing to work around — just upgrade.

The distribution is `web-cortex-framework`; the import is `webcortex`. Same
split as `djangorestframework` → `import rest_framework`.

```python
from webcortex import WebCortex   # not "web_cortex_framework"
```

## From source

```console
$ git clone https://github.com/slimboi34/web_cortex_framework
$ cd web_cortex_framework
$ uv venv --python 3.14t         # or 3.12 / 3.13 / 3.14
$ uv pip install maturin
$ .venv/bin/maturin develop --uv
```

`maturin develop` compiles the Rust extension and installs the package as
editable. First build takes a few minutes; later ones are incremental.

Verify:

```console
$ .venv/bin/webcortex --help
$ .venv/bin/python -c "import webcortex; print(webcortex.__version__)"
0.3.1
```

## Free-threaded Python

This is the difference between WebCortex being fast and being *very* fast for
Python-backed routes.

=== "uv (recommended)"

    ```console
    $ uv venv --python 3.14t          # the 't' suffix means free-threaded
    ```

=== "pyenv"

    ```console
    $ pyenv install 3.14.7t
    $ pyenv local 3.14.7t
    ```

=== "python.org installer"

    Choose the "free-threaded" variant during installation, then use
    `python3.14t`.

Check which you are on:

```python
import webcortex
print(webcortex.free_threaded())   # True on a GIL-disabled build
```

`webcortex dev` prints it at startup:

```
  python 3.14.7 (free-threaded)
```

### Why it matters

On a GIL build, N Python worker threads give concurrency for I/O and nothing for
CPU. On a free-threaded build they give real parallelism.

Measured on identical hardware with the same client driving both servers:

| Concurrency | free-threaded | GIL |
|---|---:|---:|
| 1 | 1.00x | 1.00x |
| 2 | 2.23x | 1.45x |
| 4 | 3.55x | 1.42x |
| 8 | **4.82x** | **1.38x** |

!!! info "It works either way"
    WebCortex runs correctly on a GIL build. It checks `sys._is_gil_enabled()`,
    sizes its worker pools accordingly, and tells you which mode it is in.
    Free-threading is the fast path, not a hard requirement — so adoption is
    never blocked on a dependency that has not caught up.

### The catch

C extensions without free-threading support will either fail to import or
silently re-enable the GIL. Pure-Python and Rust-backed packages are fine; the
long tail of older C extensions is not. If a dependency forces the GIL back on,
WebCortex keeps working — it just reports `free_threaded() == False`.

## Your first project

```console
$ webcortex new myapp --template api
$ cd myapp
$ export WEBCORTEX_API_KEY=$(webcortex keygen)
$ webcortex dev
```

Four starters are available:

| Template | What you get |
|---|---|
| `api` | JSON API + MCP tool surface |
| `fullstack` | The above, plus server-rendered pages and static assets |
| `agent` | The above, plus an agent with an approval-gated tool |
| `behaviour` | The above, plus Behaviours — programmable procedures |

Every starter boots with authentication, rate limiting, and security headers
already enabled. A starter that generates an insecure app teaches an insecure
habit.

[Tutorial :material-arrow-right:](tutorial.md){ .md-button .md-button--primary }
