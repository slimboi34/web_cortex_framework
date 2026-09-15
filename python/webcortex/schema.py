"""Type hints to JSON Schema.

Every schema produced here is used three times over: to document the endpoint in
OpenAPI, to generate the frontend client, and — the reason it matters most — to
tell a model what arguments a tool takes. A framework that makes you write that
schema by hand is a framework where the agent-facing description drifts out of
sync with the code. So it is derived, never authored.
"""

from __future__ import annotations

import dataclasses
import datetime as _dt
import inspect
import types
import typing
from typing import Any

__all__ = ["json_schema_for", "schema_from_signature", "coerce", "PRIMITIVES"]

PRIMITIVES: dict[Any, dict] = {
    str: {"type": "string"},
    int: {"type": "integer"},
    float: {"type": "number"},
    bool: {"type": "boolean"},
    bytes: {"type": "string", "contentEncoding": "base64"},
    _dt.date: {"type": "string", "format": "date"},
    _dt.datetime: {"type": "string", "format": "date-time"},
    type(None): {"type": "null"},
    Any: {},
}


def json_schema_for(annotation: Any) -> dict:
    """Best-effort JSON Schema for a type annotation.

    Unknown types degrade to ``{}`` (accept anything) rather than raising —
    a handler with an exotic signature should still serve traffic, it just
    won't advertise as precise a tool schema.
    """
    if annotation is inspect.Parameter.empty or annotation is None:
        return {}

    if annotation in PRIMITIVES:
        return dict(PRIMITIVES[annotation])

    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)

    # Optional[X] / X | None
    if origin in (typing.Union, types.UnionType):
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1 and len(args) == 2:
            inner = json_schema_for(non_none[0])
            return {**inner, "nullable": True} if inner else {}
        return {"anyOf": [json_schema_for(a) for a in args]}

    if origin in (list, set, frozenset, tuple):
        item = json_schema_for(args[0]) if args else {}
        return {"type": "array", "items": item}

    if origin is dict:
        value = json_schema_for(args[1]) if len(args) == 2 else {}
        return {"type": "object", "additionalProperties": value or True}

    if typing.get_origin(annotation) is typing.Literal:
        return {"enum": list(args)}

    if dataclasses.is_dataclass(annotation):
        return _dataclass_schema(annotation)

    # Pydantic and anything else exposing the standard hook.
    for attr in ("model_json_schema", "schema"):
        method = getattr(annotation, attr, None)
        if callable(method):
            try:
                return method()
            except Exception:
                break

    if isinstance(annotation, type) and issubclass(annotation, _dt.datetime):
        return {"type": "string", "format": "date-time"}

    return {}


def _dataclass_schema(cls: Any) -> dict:
    props: dict[str, dict] = {}
    required: list[str] = []
    hints = typing.get_type_hints(cls)
    for f in dataclasses.fields(cls):
        props[f.name] = json_schema_for(hints.get(f.name, f.type))
        has_default = (
            f.default is not dataclasses.MISSING
            or f.default_factory is not dataclasses.MISSING  # type: ignore[misc]
        )
        if not has_default:
            required.append(f.name)
    out: dict = {"type": "object", "title": cls.__name__, "properties": props}
    if required:
        out["required"] = required
    return out


def type_hints(fn: Any) -> dict:
    """A callable's resolved annotations, or its raw ones when a forward
    reference cannot be resolved: an exotic annotation must not stop an app
    from booting."""
    try:
        return typing.get_type_hints(fn)
    except Exception:
        return getattr(fn, "__annotations__", {})


def schema_from_signature(
    fn: Any, path_params: list[str], skip: set[str] | None = None
) -> tuple[dict, dict]:
    """Derive ``(input_schema, output_schema)`` from a handler's signature.

    The input schema is flat — path parameters, query parameters, and body
    fields all appear as sibling properties. That is deliberate: it is exactly
    the shape a model emits for a tool call, and the runtime knows how to route
    each name back to the right part of the request.
    """
    skip = skip or set()
    hints = type_hints(fn)

    sig = inspect.signature(fn)
    props: dict[str, dict] = {}
    required: list[str] = []

    for name, param in sig.parameters.items():
        if name in skip or param.kind in (
            inspect.Parameter.VAR_POSITIONAL,
            inspect.Parameter.VAR_KEYWORD,
        ):
            continue
        schema = json_schema_for(hints.get(name, param.annotation))
        if param.default is not inspect.Parameter.empty:
            try:
                schema = {**schema, "default": param.default}
            except TypeError:
                pass
        else:
            required.append(name)
        props[name] = schema

    # A path parameter is always required, even if the signature gave it a
    # default, because the URL cannot be formed without it.
    for p in path_params:
        if p not in props:
            props[p] = {"type": "string"}
        if p not in required:
            required.append(p)

    input_schema: dict = {"type": "object", "properties": props}
    if required:
        input_schema["required"] = required
    input_schema["additionalProperties"] = False

    output_schema = json_schema_for(hints.get("return", inspect.Parameter.empty))
    return input_schema, output_schema


_TRUTHY = {"1", "true", "yes", "on"}
_FALSEY = {"0", "false", "no", "off"}


def coerce(value: Any, annotation: Any) -> Any:
    """Coerce a raw path/query string to the annotated type.

    Path and query values arrive as strings; a handler annotated ``id: int``
    should receive an int. Anything we cannot convert is passed through
    unchanged so the handler can decide what to do with it.
    """
    if annotation is inspect.Parameter.empty or annotation is Any or value is None:
        return value

    origin = typing.get_origin(annotation)
    if origin in (typing.Union, types.UnionType):
        args = [a for a in typing.get_args(annotation) if a is not type(None)]
        if len(args) == 1:
            return coerce(value, args[0])
        return value

    if annotation is bool:
        if isinstance(value, bool):
            return value
        text = str(value).strip().lower()
        if text in _TRUTHY:
            return True
        if text in _FALSEY:
            return False
        return value

    if annotation in (int, float, str):
        if isinstance(value, annotation):
            return value
        try:
            return annotation(value)
        except (TypeError, ValueError):
            return value

    if dataclasses.is_dataclass(annotation) and isinstance(value, dict):
        try:
            return annotation(**value)
        except TypeError:
            return value

    return value
