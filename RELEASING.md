# Releasing

Everything is automated except the one step that requires a human logged into
PyPI. This documents both.

## One-time: link PyPI to this repository

WebCortex publishes via [trusted publishing][tp], so no API token is ever stored
in GitHub. PyPI verifies a short-lived OIDC token from the release workflow
instead. That link has to be created once, by an account owner.

1. Sign in at <https://pypi.org> (create the account if needed; 2FA is required).
2. Go to **<https://pypi.org/manage/account/publishing/>**.
3. Under *Add a new pending publisher*, enter **exactly** these values:

   | Field | Value |
   |---|---|
   | PyPI Project Name | `web-cortex-framework` |
   | Owner | `slimboi34` |
   | Repository name | `web_cortex_framework` |
   | Workflow name | `release.yml` |
   | Environment name | `pypi` |

4. Save.

It is called a *pending* publisher because the project does not exist on PyPI
yet; the first successful upload creates it and the publisher becomes permanent.

The `pypi` environment already exists on the GitHub side.

### Verifying the link worked

Re-run the release workflow (below). The `Publish to PyPI` job failing with
`invalid-publisher` means the values above do not match — most often the
repository name, which is `web_cortex_framework` with underscores, not the
`webcortex` import name.

## Cutting a release

```bash
# 1. Bump the version in all three places — they must agree.
#    pyproject.toml   version = "X.Y.Z"
#    Cargo.toml       version = "X.Y.Z"   (workspace.package)
#    python/webcortex/__init__.py  __version__ = "X.Y.Z"

# 2. Update CHANGELOG.md.

# 3. Verify locally before tagging.
cargo test && cargo clippy --all-targets -- -D warnings && cargo audit
.venv/bin/python -m pytest tests/ -q

# 4. Tag and push. The tag is what triggers publishing.
git tag -a vX.Y.Z -m "vX.Y.Z — summary"
git push origin main --tags
```

The release workflow then runs the full CI suite as a gate, builds wheels for
Linux (x86_64, aarch64), macOS (arm64, x86_64), and Windows (x64) across Python
3.12/3.13/3.14/3.14t, builds an sdist, and publishes everything.

To re-run after fixing the publisher link without cutting a new version:

```bash
gh run list --workflow Release --limit 1
gh run rerun <run-id> --failed
```

## Version numbers are permanent

PyPI does not allow re-uploading a version, even after deleting it. A broken
0.3.0 cannot be replaced by a fixed 0.3.0 — it has to become 0.3.1. Check the
artifacts before tagging:

```bash
.venv/bin/maturin sdist --out dist
.venv/bin/maturin build --release -i .venv/bin/python --out dist
.venv/bin/twine check --strict dist/*
```

`twine check --strict` catches metadata and README-rendering problems that PyPI
would otherwise reject, or silently render badly on the project page.

## Trying it against TestPyPI first

Worth doing for a first release, or any release that changes packaging.
TestPyPI is a separate instance with separate accounts and its own publisher
configuration.

1. Register the same pending publisher at
   <https://test.pypi.org/manage/account/publishing/>.
2. Add to the publish step in `.github/workflows/release.yml`:

   ```yaml
   with:
     packages-dir: dist
     repository-url: https://test.pypi.org/legacy/
   ```

3. Install from it to confirm:

   ```bash
   pip install --index-url https://test.pypi.org/simple/ \
     --extra-index-url https://pypi.org/simple/ web-cortex-framework
   ```

Remember to remove `repository-url` before the real release.

## If you would rather use an API token

Trusted publishing is preferred because nothing secret is stored. If a token is
needed anyway:

1. Create one at <https://pypi.org/manage/account/token/>, scoped to the project.
2. `gh secret set PYPI_API_TOKEN --repo slimboi34/web_cortex_framework`
3. In `release.yml`, drop `environment: pypi` and the `id-token` permission, and
   pass the token to the publish action:

   ```yaml
   with:
     packages-dir: dist
     password: ${{ secrets.PYPI_API_TOKEN }}
   ```

[tp]: https://docs.pypi.org/trusted-publishers/
