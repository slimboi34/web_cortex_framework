# Releasing

```bash
# 1. Bump the version. All four must agree.
#    pyproject.toml                version = "X.Y.Z"
#    Cargo.toml                    version = "X.Y.Z"   (workspace.package)
#    Cargo.lock                    webcortex-core and webcortex-py
#    python/webcortex/__init__.py  __version__ = "X.Y.Z"
#
#    `cargo check` updates Cargo.lock for you once the other two are edited.

# 2. Update CHANGELOG.md.

# 3. Tag. The tag is what publishes.
git tag -a vX.Y.Z -m "vX.Y.Z — summary"
git push origin main --tags
```

That is the whole process. The workflow runs CI as a gate, and publishes the
wheels and sdist that CI built — it does not rebuild them, so the artifact that
was tested is the artifact that ships.

Supported interpreters are **3.12, 3.13, and free-threaded 3.14 (`3.14t`)**
across Linux (x86_64, aarch64), macOS (arm64, x86_64) and Windows (x64).
GIL-enabled 3.14 is deliberately excluded — see [AGENTS.md §11](AGENTS.md).

`workflow_dispatch` runs the same thing without a tag, which is useful for
re-running a failed publish. It is a no-op if the version is already on PyPI.

## Version numbers are permanent

PyPI does not allow re-uploading a file, even after deleting it. A broken 0.3.0
cannot be replaced by a fixed 0.3.0 — it has to become 0.3.1. This is not
hypothetical: 0.3.0 shipped without an sdist because its sdist was rejected, and
that could only be corrected by releasing again.

So check before tagging:

```bash
maturin sdist --out dist
maturin build --release --out dist
twine check --strict dist/*
```

`twine check --strict` catches metadata and README-rendering problems that PyPI
would otherwise reject.

## PyPI setup

Already configured, via [trusted publishing][tp] — no API token is stored
anywhere. Recorded here in case it ever needs rebuilding:

| Field | Value |
|---|---|
| PyPI Project Name | `web-cortex-framework` |
| Owner | `slimboi34` |
| Repository name | `web_cortex_framework` |
| Workflow name | `release.yml` |
| Environment name | `pypi` |

If `Publish to PyPI` ever fails with `invalid-publisher`, one of those five no
longer matches. The usual culprit is the repository name, which is
`web_cortex_framework` with underscores — not the `webcortex` import name.

Leave *Environment name* set to `pypi` rather than *(Any)*. With `(Any)`, any
job in `release.yml` can mint a publishing token; with `pypi`, only jobs that
declare `environment: pypi` can, which is what makes GitHub's environment
protection rules actually binding.

[tp]: https://docs.pypi.org/trusted-publishers/
