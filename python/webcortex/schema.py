"""Type hints to JSON Schema.

Every schema produced here is used three times over: to document the endpoint in
OpenAPI, to generate the frontend client, and — the reason it matters most — to
tell a model what arguments a tool takes. A framework that makes you write that
schema by hand is a framework where the agent-facing description drifts out of
sync with the code. So it is derived, never authored.
"""

from __future__ import annotations

import base64
import binascii
import dataclasses
import datetime as _dt
import decimal
import enum
import functools
import inspect
import json
import math
import sys
import types
import typing
import uuid
from typing import Any

__all__ = ["json_schema_for", "output_schema_for", "schema_from_signature", "coerce", "PRIMITIVES"]

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

# Types the response encoder writes as strings (`_bridge._fallback`), so the
# schema says what the wire carries.
_STRING_FORMATS: dict[Any, dict] = {
    _dt.time: {"type": "string", "format": "time"},
    _dt.timedelta: {"type": "number"},
    uuid.UUID: {"type": "string", "format": "uuid"},
    decimal.Decimal: {"type": "string", "format": "decimal"},
    bytearray: {"type": "string", "contentEncoding": "base64"},
}

_BARE_CONTAINERS = {list: "array", set: "array", frozenset: "array", tuple: "array", dict: "object"}


class _Gen:
    """How one schema is being generated.

    ``closed``: a dataclass or ``TypedDict`` object refuses keys it does not
    declare (``additionalProperties: false``). ``refs``: a class that contains
    itself is described once under ``$defs`` at the root and referenced with
    ``{"$ref": "#/$defs/<Name>"}`` where it recurs, instead of the loose
    ``{"type": "object"}`` that stops validation one level down.

    Both are for a typed answer (``output=``), which the runtime validates.
    Route schemas are advertisement for OpenAPI, the TypeScript client and
    tool lists, and keep their established shape.
    """

    __slots__ = ("closed", "refs", "defs", "names", "recursive")

    def __init__(self, *, closed: bool = False, refs: bool = False) -> None:
        self.closed = closed
        self.refs = refs
        self.defs: dict[str, dict] = {}
        self.names: dict[Any, str] = {}
        self.recursive: set = set()

    def def_name(self, cls: Any) -> str:
        """A ``$defs`` key for ``cls``: its name, made unique per schema."""
        name = self.names.get(cls)
        if name is None:
            base = "".join(c if c.isalnum() or c == "_" else "_"
                           for c in getattr(cls, "__name__", "") or "") or "Type"
            taken = set(self.names.values())
            name, n = base, 2
            while name in taken:
                name, n = f"{base}_{n}", n + 1
            self.names[cls] = name
        return name

    def recursion_point(self, cls: Any) -> dict:
        """What to emit where ``cls`` occurs inside its own schema."""
        if self.refs:
            self.recursive.add(cls)
            return {"$ref": f"#/$defs/{self.def_name(cls)}"}
        return {"type": "object", "title": cls.__name__}

    def finish(self, cls: Any, schema: dict) -> dict:
        if self.refs and cls in self.recursive:
            self.defs[self.def_name(cls)] = schema
        return schema


def json_schema_for(annotation: Any) -> dict:
    """Best-effort JSON Schema for a type annotation.

    Unknown types degrade to ``{}`` (accept anything) rather than raising —
    a handler with an exotic signature should still serve traffic, it just
    won't advertise as precise a tool schema.
    """
    return _schema(annotation, (), _Gen())


def output_schema_for(annotation: Any) -> dict:
    """JSON Schema for a typed answer: ``app.agent(output=...)``.

    The same rules as :func:`json_schema_for`, plus two that only matter to a
    schema the runtime validates against:

    - a dataclass, and a ``TypedDict`` not declared open (PEP 728
      ``closed=False`` / ``extra_items=``), refuses keys it does not declare
      (``additionalProperties: false``), at every level; ``dict[str, X]``
      stays open;
    - a recursive type (``children: list["Node"]``) is described once under
      ``$defs`` at the root, and each recursion point is
      ``{"$ref": "#/$defs/Node"}``. A type that does not recur has no
      ``$defs`` and keeps its inline shape.
    """
    gen = _Gen(closed=True, refs=True)
    out = _schema(annotation, (), gen)
    # A class whose schema failed part-way (and degraded to `{}`) never got
    # its `$defs` entry; a `$ref` to it would point at nothing.
    dangling = {gen.def_name(c) for c in gen.recursive} - set(gen.defs)
    if dangling:
        out = _loosen_refs(out, dangling)
        gen.defs = {k: _loosen_refs(v, dangling) for k, v in gen.defs.items()}
    if gen.defs and out:
        out = {**out, "$defs": dict(gen.defs)}
    return out


def _loosen_refs(schema: Any, names: set[str]) -> Any:
    """``schema`` with each ``#/$defs/<name>`` for ``names`` a loose object."""
    if isinstance(schema, list):
        return [_loosen_refs(s, names) for s in schema]
    if not isinstance(schema, dict):
        return schema
    ref = schema.get("$ref")
    if isinstance(ref, str) and ref.startswith("#/$defs/") and ref[len("#/$defs/"):] in names:
        return {"type": "object", "title": ref[len("#/$defs/"):]}
    return {k: _loosen_refs(v, names) for k, v in schema.items()}


def _schema(annotation: Any, active: tuple, gen: _Gen) -> dict:
    try:
        return _schema_inner(annotation, active, gen)
    except Exception:
        # A schema is advertisement, never a reason to refuse to boot.
        return {}


def _schema_inner(annotation: Any, active: tuple, gen: _Gen) -> dict:
    if annotation is inspect.Parameter.empty or annotation is None:
        return {}

    try:
        primitive = PRIMITIVES.get(annotation)
    except TypeError:  # an unhashable "annotation"
        return {}
    if primitive is not None:
        return dict(primitive)

    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)

    if origin is typing.Annotated:
        return _schema(args[0], active, gen)

    # `UserId = NewType("UserId", int)` is its supertype on the wire.
    supertype = getattr(annotation, "__supertype__", None)
    if supertype is not None:
        return _schema(supertype, active, gen)

    # Optional[X] / X | None
    if origin in (typing.Union, types.UnionType):
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1 and len(args) == 2:
            inner = _schema(non_none[0], active, gen)
            if not inner:
                return {}
            if "$ref" in inner:
                # `parent: Node | None` at a recursion point. A sibling of
                # `$ref` may be ignored by whoever resolves it; anyOf is not.
                return {"anyOf": [inner, {"type": "null"}]}
            out = {**inner, "nullable": True}
            if "enum" in out and None not in out["enum"]:
                out["enum"] = [*out["enum"], None]
            return out
        return {"anyOf": [_schema(a, active, gen) for a in args]}

    if origin is tuple:
        if len(args) == 2 and args[1] is Ellipsis:
            return {"type": "array", "items": _schema(args[0], active, gen)}
        if args == ((),):  # tuple[()]
            return {"type": "array", "maxItems": 0}
        if args:
            # A fixed-length tuple is positional: `tuple[int, str]` is not a
            # list of ints.
            return {"type": "array", "prefixItems": [_schema(a, active, gen) for a in args],
                    "minItems": len(args), "maxItems": len(args)}
        return {"type": "array"}

    if origin in (list, set, frozenset) or (
        isinstance(origin, type) and origin is not dict and _is_sequence_abc(origin)
    ):
        out = {"type": "array", "items": _schema(args[0], active, gen) if args else {}}
        if origin in (set, frozenset) or origin is getattr(typing, "AbstractSet", None):
            out["uniqueItems"] = True
        return out

    if origin is dict or (isinstance(origin, type) and _is_mapping_abc(origin)):
        value = _schema(args[1], active, gen) if len(args) == 2 else {}
        return {"type": "object", "additionalProperties": value or True}

    if origin is typing.Literal:
        return {"enum": [_literal_value(a) for a in args]}

    if annotation in _BARE_CONTAINERS:
        return {"type": _BARE_CONTAINERS[annotation]}

    if isinstance(annotation, type) and issubclass(annotation, enum.Enum):
        values = [_literal_value(m) for m in annotation]
        out: dict = {"enum": values, "title": annotation.__name__}
        if values and all(isinstance(v, str) for v in values):
            out["type"] = "string"
        elif values and all(isinstance(v, int) and not isinstance(v, bool) for v in values):
            out["type"] = "integer"
        return out

    if dataclasses.is_dataclass(annotation) and isinstance(annotation, type):
        return _dataclass_schema(annotation, active, gen)

    if _is_typeddict(annotation):
        return _typeddict_schema(annotation, active, gen)

    # Pydantic and anything else exposing the standard hook.
    for attr in ("model_json_schema", "schema"):
        method = getattr(annotation, attr, None)
        if callable(method):
            try:
                return method()
            except Exception:
                break

    if isinstance(annotation, type):
        if issubclass(annotation, _dt.datetime):
            return {"type": "string", "format": "date-time"}
        for base, schema in _STRING_FORMATS.items():
            if issubclass(annotation, base):
                return dict(schema)
        # Subclasses of the primitives (`class Name(str)`) are their base.
        for base in (bool, int, float, str, bytes):
            if issubclass(annotation, base):
                return dict(PRIMITIVES[base])

    return {}


def _is_sequence_abc(origin: type) -> bool:
    import collections.abc as abc

    return origin in (abc.Sequence, abc.MutableSequence, abc.Iterable, abc.Collection,
                      abc.Set, abc.MutableSet, abc.Iterator, abc.Generator)


def _is_mapping_abc(origin: type) -> bool:
    import collections.abc as abc

    return origin in (abc.Mapping, abc.MutableMapping)


def _literal_value(value: Any) -> Any:
    """A `Literal`/`Enum` value as JSON: an enum member is its value."""
    if isinstance(value, enum.Enum):
        value = value.value
    if value is None or isinstance(value, (str, bool, int)):
        return value
    if isinstance(value, float) and math.isfinite(value):
        return value
    return str(value)


def _is_typeddict(annotation: Any) -> bool:
    is_td = getattr(typing, "is_typeddict", None)
    if is_td and is_td(annotation):
        return True
    # typing_extensions.TypedDict (how PEP 728's closed=/extra_items= are
    # spelled before Python 3.15) has its own metaclass. Only consulted when
    # the app already imported it.
    te = sys.modules.get("typing_extensions")
    te_is_td = getattr(te, "is_typeddict", None)
    return bool(te_is_td and te_is_td(annotation))


def _class_hints(cls: Any, *, include_extras: bool = False) -> dict:
    """`get_type_hints`, falling back per field when one forward reference
    does not resolve (so the others still do).

    ``include_extras`` keeps ``Annotated``, ``Required``, ``NotRequired`` and
    ``ReadOnly``; the per-field fallback always keeps them (it evaluates the
    annotation text as written)."""
    try:
        return typing.get_type_hints(cls, include_extras=include_extras)
    except Exception:
        pass
    module = sys.modules.get(getattr(cls, "__module__", ""), None)
    globalns = dict(vars(module)) if module else {}
    globalns.setdefault(cls.__name__, cls)
    out: dict = {}
    for klass in reversed(getattr(cls, "__mro__", [cls])):
        for name, value in getattr(klass, "__annotations__", {}).items():
            out[name] = _resolve(value, globalns)
    return out


def _resolve(value: Any, globalns: dict) -> Any:
    # A quoted annotation under `from __future__ import annotations` is a
    # string of a string ("'int'"): evaluate until it is not a string.
    for _ in range(3):
        if isinstance(value, typing.ForwardRef):
            value = value.__forward_arg__
        if not isinstance(value, str):
            return value
        try:
            value = eval(value, globalns, {})  # what get_type_hints does
        except Exception:
            return value
    return value


def _dataclass_schema(cls: Any, active: tuple, gen: _Gen) -> dict:
    if cls in active:
        # A recursive type (`children: list["Node"]`): a `$ref` for a typed
        # answer, otherwise a loose object, instead of recursing until the
        # stack runs out.
        return gen.recursion_point(cls)
    active = (*active, cls)
    props: dict[str, dict] = {}
    required: list[str] = []
    hints = _class_hints(cls)
    for f in dataclasses.fields(cls):
        props[f.name] = _schema(hints.get(f.name, f.type), active, gen)
        has_default = (
            f.default is not dataclasses.MISSING
            or f.default_factory is not dataclasses.MISSING  # type: ignore[misc]
        )
        if not has_default:
            required.append(f.name)
    out: dict = {"type": "object", "title": cls.__name__, "properties": props}
    if required:
        out["required"] = required
    if gen.closed:
        out["additionalProperties"] = False
    return gen.finish(cls, out)


def _typeddict_schema(cls: Any, active: tuple, gen: _Gen) -> dict:
    if cls in active:
        return gen.recursion_point(cls)
    active = (*active, cls)
    props: dict[str, dict] = {}
    required: list[str] = []
    for name, (annotation, is_required) in typeddict_fields(cls).items():
        props[name] = _schema(annotation, active, gen)
        if is_required:
            required.append(name)
    out: dict = {"type": "object", "title": cls.__name__, "properties": props}
    if required:
        out["required"] = required
    if gen.closed:
        extra = _typeddict_extra_items(cls)
        if extra is None:
            out["additionalProperties"] = False
        elif extra is True:
            out["additionalProperties"] = True
        else:
            out["additionalProperties"] = _schema(extra, active, gen) or True
    return gen.finish(cls, out)


def typeddict_fields(cls: Any) -> dict[str, tuple[Any, bool]]:
    """``{key: (annotation, required)}`` for a ``TypedDict``, qualifiers removed.

    ``cls.__required_keys__`` is wrong under ``from __future__ import
    annotations``: the qualifiers are still strings when the class is built,
    so ``NotRequired[str]`` counts as required in a total class and
    ``Required[int]`` as optional in a ``total=False`` one. The qualifiers
    are read here from the resolved hints instead. A key with neither
    qualifier is required when the class that declared it is total, which is
    the one thing ``__required_keys__`` does get right in that case.
    """
    hints = _class_hints(cls, include_extras=True)
    declared = getattr(cls, "__required_keys__", None)
    total = bool(getattr(cls, "__total__", True))
    out: dict[str, tuple[Any, bool]] = {}
    for name, annotation in hints.items():
        inner, qualifiers = _unwrap_typeddict_field(annotation)
        if "Required" in qualifiers:
            is_required = True
        elif "NotRequired" in qualifiers:
            is_required = False
        elif declared is not None:
            is_required = name in declared
        else:
            is_required = total
        out[name] = (inner, is_required)
    return out


_QUALIFIER_NAMES = ("Required", "NotRequired", "ReadOnly")


def _qualifier(origin: Any) -> str | None:
    """``"Required"``, ``"NotRequired"`` or ``"ReadOnly"`` when ``origin`` is
    that qualifier from ``typing`` or (when loaded) ``typing_extensions``."""
    if origin is None:
        return None
    for module in (typing, sys.modules.get("typing_extensions")):
        for name in _QUALIFIER_NAMES:
            if module is not None and getattr(module, name, None) is origin:
                return name
    return None


def _unwrap_typeddict_field(annotation: Any) -> tuple[Any, set[str]]:
    """``Annotated[Required[ReadOnly[int]], …]`` → ``(int, {"Required", "ReadOnly"})``.

    Metadata on ``Annotated`` is dropped; the schema never used it."""
    qualifiers: set[str] = set()
    for _ in range(8):
        origin = typing.get_origin(annotation)
        if origin is typing.Annotated:
            annotation = typing.get_args(annotation)[0]
        elif (name := _qualifier(origin)) is not None:
            qualifiers.add(name)
            annotation = typing.get_args(annotation)[0]
        elif isinstance(annotation, str) and annotation.endswith("]") and \
                annotation.split("[", 1)[0].rsplit(".", 1)[-1] in _QUALIFIER_NAMES:
            # An annotation that never resolved (a typo elsewhere in it): its
            # qualifier is still readable, and the rest stays a string.
            head, rest = annotation.split("[", 1)
            qualifiers.add(head.rsplit(".", 1)[-1])
            annotation = rest[:-1]
        else:
            break
    return annotation, qualifiers


def _typeddict_extra_items(cls: Any) -> Any:
    """What extra keys a ``TypedDict`` allows: ``None`` (none: it is closed),
    ``True`` (any) or a type, from PEP 728's ``closed=`` / ``extra_items=``.

    A ``TypedDict`` that says neither is treated as closed: it lists the
    keys the answer has, and a typed answer with keys nobody declared is the
    mistake the check exists to catch."""
    extra = getattr(cls, "__extra_items__", None)
    # typing_extensions spells "no extra_items= given" as a NoExtraItems sentinel.
    if extra is not None and "NoExtraItems" not in repr(extra):
        return extra
    if getattr(cls, "__closed__", None) is False:
        return True
    return None


def type_hints(fn: Any) -> dict:
    """A callable's resolved annotations.

    When one forward reference cannot be resolved (a name imported only under
    ``TYPE_CHECKING``, a typo), the others are still resolved one by one:
    under ``from __future__ import annotations`` every annotation is a
    string, and giving up on all of them would silently stop ``id: int``
    from being coerced. An exotic annotation must not stop an app from
    booting either, so nothing here raises.
    """
    target = fn
    while isinstance(target, functools.partial):
        target = target.func
    if not (inspect.isroutine(target) or isinstance(target, type)) and callable(target):
        target = getattr(target, "__call__", target)  # a callable object: its __call__
    try:
        return typing.get_type_hints(target)
    except Exception:
        pass
    raw = getattr(target, "__annotations__", None) or {}
    globalns = getattr(inspect.unwrap(target), "__globals__", None)
    if globalns is None:
        globalns = getattr(getattr(target, "__func__", None), "__globals__", {})
    return {name: _resolve(value, globalns) for name, value in dict(raw).items()}


def _json_default(value: Any) -> tuple[bool, Any]:
    """A parameter default as JSON, or ``(False, None)`` when it has no JSON
    spelling (a sentinel ``object()``, a function): the manifest is JSON, and
    one unserialisable default would make the whole app fail to boot."""
    if isinstance(value, enum.Enum):
        value = value.value
    elif isinstance(value, (_dt.datetime, _dt.date, _dt.time)):
        value = value.isoformat()
    elif isinstance(value, (set, frozenset)):
        value = list(value)
    try:
        json.dumps(value, allow_nan=False)
    except (TypeError, ValueError):
        return False, None
    return True, value


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
            ok, default = _json_default(param.default)
            if ok:
                schema = {**schema, "default": default}
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
    """Coerce a raw path/query string (or JSON body value) to the annotation.

    Path and query values arrive as strings; a handler annotated ``id: int``
    should receive an int, ``day: date`` a ``date``, ``colour: Colour`` the
    enum member, and ``items: list[Item]`` a list of ``Item``. Anything we
    cannot convert is passed through unchanged so the handler can decide
    what to do with it. Conversions never lose information: ``3.7`` is not
    an ``int``, ``2**53 + 1`` is not a ``float`` (no float equals it) and a
    JSON object is not a ``str``, so all three pass through.
    """
    try:
        return _coerce(value, annotation, 0)
    except (RecursionError, ArithmeticError, ValueError, TypeError):
        # A conversion that failed in a way no branch foresaw passes the value
        # through, as promised: through 2.9.0 an int past the float range
        # (OverflowError) or past the int/str digit limit (ValueError) made
        # any client's request a 500 before the handler ran.
        return value


def _coerce(value: Any, annotation: Any, depth: int) -> Any:
    if annotation is inspect.Parameter.empty or annotation is Any or value is None or depth > 32:
        return value

    origin = typing.get_origin(annotation)
    if origin is typing.Annotated:
        return _coerce(value, typing.get_args(annotation)[0], depth)
    supertype = getattr(annotation, "__supertype__", None)
    if supertype is not None:
        return _coerce(value, supertype, depth)

    if origin in (typing.Union, types.UnionType):
        args = [a for a in typing.get_args(annotation) if a is not type(None)]
        if len(args) == 1:
            return _coerce(value, args[0], depth)
        return value

    if origin is typing.Literal:
        choices = typing.get_args(annotation)
        if any(value == c and type(value) is type(c) for c in choices):
            return value
        # A query string carries `1` for `Literal[1, 2]` as "1".
        for c in choices:
            if not isinstance(c, bool) and str(_literal_value(c)) == str(value):
                return c
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

    if annotation is str:
        if isinstance(value, str):
            return value
        # A number sent for a string field is still that number's text; an
        # object or a list is not, and repr() of it would be a made-up value.
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            return str(value)
        return value

    if annotation is int:
        if isinstance(value, int):
            return value
        if isinstance(value, float):
            return int(value) if value.is_integer() else value
        if isinstance(value, str):
            try:
                return int(value)
            except ValueError:
                return value
        return value

    if annotation is float:
        if isinstance(value, float):
            return value
        if isinstance(value, int) and not isinstance(value, bool):
            # Only a float that equals the int: 2**53 + 1 has none (it would
            # come out as 2**53), and 10**400 is past the float range.
            try:
                number = float(value)
            except OverflowError:
                return value
            return number if number == value else value
        if isinstance(value, str):
            try:
                number = float(value)
            except ValueError:
                return value
            # "nan" and "inf" parse, but no JSON Schema number is either.
            return number if math.isfinite(number) else value
        return value

    if isinstance(annotation, type):
        if issubclass(annotation, enum.Enum):
            return _coerce_enum(value, annotation)
        if annotation is _dt.datetime or annotation is _dt.date or annotation is _dt.time:
            if isinstance(value, annotation) and (annotation is not _dt.date
                                                  or not isinstance(value, _dt.datetime)):
                return value
            if isinstance(value, str):
                try:
                    return annotation.fromisoformat(value)
                except ValueError:
                    return value
            return value
        if annotation is uuid.UUID and isinstance(value, str):
            try:
                return uuid.UUID(value)
            except ValueError:
                return value
        if annotation is decimal.Decimal and isinstance(value, (str, int, float)) \
                and not isinstance(value, bool):
            try:
                number = decimal.Decimal(str(value))
            except decimal.InvalidOperation:
                return value
            return number if number.is_finite() else value
        if annotation is bytes and isinstance(value, str):
            # The schema advertises base64 for `bytes`.
            try:
                return base64.b64decode(value, validate=True)
            except (binascii.Error, ValueError):
                return value

    args = typing.get_args(annotation)
    if origin in (list, set, frozenset) and isinstance(value, list):
        item = args[0] if args else Any
        items = [_coerce(v, item, depth + 1) for v in value]
        if origin is list:
            return items
        try:
            return origin(items)
        except TypeError:  # unhashable items
            return value
    if origin is tuple and isinstance(value, list):
        if len(args) == 2 and args[1] is Ellipsis:
            return tuple(_coerce(v, args[0], depth + 1) for v in value)
        if args and len(args) == len(value):
            return tuple(_coerce(v, a, depth + 1) for v, a in zip(value, args))
        return tuple(value) if not args else value
    if origin is dict and isinstance(value, dict) and len(args) == 2:
        return {k: _coerce(v, args[1], depth + 1) for k, v in value.items()}

    if dataclasses.is_dataclass(annotation) and isinstance(annotation, type) and isinstance(value, dict):
        hints = _class_hints(annotation)
        names = {f.name for f in dataclasses.fields(annotation) if f.init}
        if not set(value) <= names:
            return value
        try:
            return annotation(**{k: _coerce(v, hints.get(k, Any), depth + 1) for k, v in value.items()})
        except (TypeError, ValueError):
            return value

    if _is_typeddict(annotation) and isinstance(value, dict):
        fields = typeddict_fields(annotation)
        return {k: _coerce(v, fields[k][0], depth + 1) if k in fields else v
                for k, v in value.items()}

    return value


def _coerce_enum(value: Any, annotation: type) -> Any:
    if isinstance(value, annotation):
        return value
    try:
        return annotation(value)
    except (ValueError, TypeError):
        pass
    for member in annotation:  # type: ignore[attr-defined]
        if isinstance(value, str) and (member.name == value or str(member.value) == value):
            return member
    return value
