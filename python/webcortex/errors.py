"""The error catalogue: stable ``WCnnn`` codes for every failure a user meets.

The table lives in Rust (``crates/webcortex-core/src/errors.rs``) and is read
here through ``_core.error_catalogue()``, so the runtime and the CLI can never
disagree about what a code means. ``webcortex explain CODE`` prints an entry;
``webcortex explain --list`` prints them all.

Ranges: ``WC0xx`` internal, ``WC1xx`` configuration, ``WC2xx`` routing/HTTP,
``WC3xx`` agents and providers, ``WC4xx`` budgets and limits, ``WC5xx``
devices and actuators, ``WC6xx`` authentication and security.
"""

from __future__ import annotations

import json
import re
from functools import lru_cache
from typing import Any

__all__ = ["catalogue", "lookup", "hint", "code_for_status", "error_body", "format_error", "CODE_RE"]

CODE_RE = re.compile(r"\bWC\d{3}\b")

DOCS_URL = "https://slimboi34.github.io/web_cortex_framework/error-codes/"

_STATUS_DEFAULTS = {
    400: "WC205", 401: "WC601", 403: "WC602", 404: "WC207", 405: "WC202", 408: "WC216", 409: "WC214",
    413: "WC204", 422: "WC206", 423: "WC502", 426: "WC213", 429: "WC401", 500: "WC001",
    502: "WC300", 503: "WC215", 504: "WC203", 508: "WC403",
}


@lru_cache(maxsize=1)
def catalogue() -> tuple[dict, ...]:
    """Every entry: ``{code, title, category, status, hint}``, ordered by code."""
    from . import _core

    return tuple(json.loads(_core.error_catalogue()))


def lookup(code: str) -> dict | None:
    code = code.strip().upper()
    return next((e for e in catalogue() if e["code"] == code), None)


def hint(code: str) -> str:
    entry = lookup(code)
    return entry["hint"] if entry else ""


def code_for_status(status: int) -> str:
    """The generic code for an HTTP status (mirrors ``errors::code_for_status``)."""
    return _STATUS_DEFAULTS.get(int(status), "WC200")


def error_body(status: int, message: str, code: str | None = None, hint_text: str | None = None) -> dict:
    """The JSON error body the runtime emits, built in Python."""
    code = code or code_for_status(status)
    return {"error": {"status": status, "message": message, "code": code,
                      "hint": hint_text if hint_text is not None else hint(code)}}


def format_error(code: str, message: str, hint_text: str | None = None) -> str:
    """The CLI's human form: ``error[WC101]: message`` then ``hint: …``."""
    text = f"error[{code}]: {message}"
    h = hint_text if hint_text is not None else hint(code)
    if h:
        text += f"\nhint: {h}"
    text += f"\nsee: webcortex explain {code}"
    return text


def explain(code: str) -> dict[str, Any] | None:
    entry = lookup(code)
    if entry is None:
        return None
    return {**entry, "docs": f"{DOCS_URL}#{entry['code'].lower()}"}
