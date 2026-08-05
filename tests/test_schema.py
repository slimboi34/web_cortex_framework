"""Schema derivation is load-bearing: it is what a model reads before calling a
tool. A wrong schema is a wrong tool call, so these are not cosmetic tests."""

from __future__ import annotations

import dataclasses
from typing import Literal, Optional

import pytest

from rango.schema import coerce, json_schema_for, schema_from_signature


def test_primitives():
    assert json_schema_for(int) == {"type": "integer"}
    assert json_schema_for(str) == {"type": "string"}
    assert json_schema_for(bool) == {"type": "boolean"}
    assert json_schema_for(float) == {"type": "number"}


def test_optional_collapses_to_nullable():
    assert json_schema_for(Optional[int]) == {"type": "integer", "nullable": True}
    assert json_schema_for(int | None) == {"type": "integer", "nullable": True}


def test_containers():
    assert json_schema_for(list[str]) == {"type": "array", "items": {"type": "string"}}
    assert json_schema_for(dict[str, int]) == {
        "type": "object",
        "additionalProperties": {"type": "integer"},
    }


def test_literal_becomes_enum():
    assert json_schema_for(Literal["a", "b"]) == {"enum": ["a", "b"]}


def test_dataclass():
    @dataclasses.dataclass
    class Book:
        title: str
        year: int = 2000

    schema = json_schema_for(Book)
    assert schema["type"] == "object"
    assert schema["properties"]["title"] == {"type": "string"}
    assert schema["required"] == ["title"], "fields with defaults must not be required"


def test_unknown_type_degrades_instead_of_raising():
    class Weird:
        pass

    assert json_schema_for(Weird) == {}


def test_signature_to_schema_flattens_path_and_query():
    def handler(id: int, style: str = "plain") -> str: ...

    schema, out = schema_from_signature(handler, path_params=["id"])
    assert schema["properties"]["id"] == {"type": "integer"}
    assert schema["properties"]["style"] == {"type": "string", "default": "plain"}
    assert schema["required"] == ["id"]
    assert schema["additionalProperties"] is False
    assert out == {"type": "string"}


def test_path_params_are_required_even_with_a_default():
    def handler(id: int = 1) -> None: ...

    schema, _ = schema_from_signature(handler, path_params=["id"])
    assert "id" in schema["required"]


def test_skipped_params_are_absent():
    def handler(req, id: int) -> None: ...

    schema, _ = schema_from_signature(handler, path_params=["id"], skip={"req"})
    assert "req" not in schema["properties"]


@pytest.mark.parametrize(
    "raw,annotation,expected",
    [
        ("42", int, 42),
        ("3.5", float, 3.5),
        ("true", bool, True),
        ("off", bool, False),
        (7, str, "7"),
        ("nope", int, "nope"),  # uncoercible passes through untouched
        ("x", Optional[str], "x"),
    ],
)
def test_coerce(raw, annotation, expected):
    assert coerce(raw, annotation) == expected
