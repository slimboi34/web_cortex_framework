"""`reasoning=`: carried in the manifest for agents and behaviours, validated at declaration."""
from __future__ import annotations

import pytest

from webcortex import WebCortex


def test_reasoning_rides_in_the_manifest():
    app = WebCortex("t")
    app.agent("eye", model="ollama/qwen3.5:4b", reasoning="none")
    app.agent("quiet")

    @app.behaviour("plan", reasoning="low")
    def plan(ctx, input):
        return {}

    manifest = app.manifest()
    agents = {a["name"]: a for a in manifest["agents"]}
    assert agents["eye"]["reasoning"] == "none"
    assert agents["quiet"]["reasoning"] is None, "unset means the provider's default, so nothing is sent"
    assert next(b for b in manifest["behaviours"] if b["name"] == "plan")["reasoning"] == "low"


def test_reasoning_must_be_a_known_level():
    app = WebCortex("t")
    with pytest.raises(ValueError, match="reasoning must be one of none, low, medium, high"):
        app.agent("eye", reasoning="max")
    with pytest.raises(ValueError, match="behaviour 'plan'"):
        app.behaviour("plan", reasoning="off")(lambda ctx, input: {})
