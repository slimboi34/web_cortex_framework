"""Evals: regression tests for what an agent application does.

An eval is one call to a tool (an agent, a flow, a behaviour, any route) and a
set of statements about the result: which agents the run passed through, which
tools it called, what the answer contains, how many tokens the whole tree
spent. ``webcortex eval`` runs them in-process, with no port bound, and exits 1
when one fails::

    app.eval("refund_goes_to_billing", target="front_desk",
             input="I was charged twice",
             expect={"path": ["front_desk", "billing"],
                     "tools_called": ["lookup_invoice"],
                     "max_tokens": 6000})

**Cassettes.** ``webcortex eval --record`` runs each eval against the real
providers and writes the model's responses to ``evals/<name>.json``. From then
on the eval *replays* them: no network, no key, no tokens, and the same answer
every time, so it can run in CI on every commit. What is replayed is only the
model. The tools, scopes, approval gates, handoffs, budgets and flows around it
run for real, which is the half of an agent application that is your code.

A cassette is tied to the agent it was recorded for. Each recorded response
carries a fingerprint of the agent's system prompt, model, tools (names,
descriptions and schemas) and request options; change any of them and the
replay fails with ``WC319`` instead of answering a question that is no longer
the one asked. Record again, read the diff of the cassette, and commit it:
that diff is the review of what your prompt change did.

Checks are deterministic on purpose. There is no model grading another model
here; ``check=`` takes a function when a statement needs code.
"""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Sequence

__all__ = ["EvalCase", "declare", "run_evals", "check_result", "render_report", "cassette_path",
           "EXPECT_KEYS", "MODES"]

#: Every key ``expect=`` understands. An unknown key is a ``ValueError`` at
#: declaration, so a typo cannot turn a check into nothing.
EXPECT_KEYS = (
    "status", "http_status", "code", "output_contains", "output_not_contains", "output_matches",
    "data", "tools_called", "tools_not_called", "path", "max_steps", "max_tokens",
    "max_model_calls",
)

#: The modes ``run_evals`` and ``webcortex eval`` accept.
MODES = ("auto", "replay", "live", "record")

_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")

# Tool spans with these codes are calls the runtime refused before the tool
# ran, not calls that ran: a tool the agent was not given (WC307), arguments
# that are not an object (WC317), a caller without the scope or credential
# (WC601-WC604, WC607), the emergency stop (WC502), a rate limit (WC401) and
# nesting too deep (WC403).
_REFUSED = frozenset({"WC307", "WC317", "WC401", "WC403", "WC502",
                      "WC601", "WC602", "WC603", "WC604", "WC607"})

# Checks that can only be answered from the run's trace.
_TRACE_KEYS = ("tools_called", "tools_not_called", "max_model_calls")

# A model call that failed at the provider: no response was obtained, so a
# recording made from that run would be incomplete.
_PROVIDER_CODES = frozenset({"WC301", "WC302", "WC303", "WC304", "WC305"})


@dataclass
class EvalCase:
    """One declared eval. Built by :meth:`webcortex.WebCortex.eval`.

    Attributes:
        name: Unique within the app; the cassette is ``<directory>/<name>.json``.
        target: The tool that is called.
        args: The arguments sent, always a dict.
        expect: The declared checks (keys from :data:`EXPECT_KEYS`).
        check: An optional function of the result for checks that need code.
        scopes: The scopes the calling principal (``eval``) holds.
        description: One line for the report.

    Example::

        case = EvalCase("greets", target="desk", args={"input": "hi"},
                        expect={"output_contains": "hello"})
    """

    name: str
    target: str
    args: dict
    expect: dict = field(default_factory=dict)
    check: Callable[[dict], Any] | None = None
    scopes: list = field(default_factory=list)
    description: str = ""


def declare(app: Any, name: str, *, target: str, input: Any, expect: dict | None = None,
            check: Callable[[dict], Any] | None = None, scopes: Sequence[str] = (),
            description: str = "") -> EvalCase:
    """Validate an eval's declaration and return the case.

    Called by :meth:`webcortex.WebCortex.eval`; use that rather than this.

    Args:
        app: The app the eval belongs to (its other evals are checked for a
            duplicate name).
        name: Letters, digits, ``_``, ``.`` and ``-``; it becomes a file name.
        target: The tool to call.
        input: A dict of arguments, or a string as shorthand for
            ``{"input": <string>}``.
        expect: Checks, keyed by :data:`EXPECT_KEYS`.
        check: A function of the result.
        scopes: Scopes the caller holds.
        description: One line for the report.

    Returns:
        The :class:`EvalCase`.

    Raises:
        ValueError: for a bad name, a duplicate, an input that is neither a
            dict nor a string, an unknown ``expect`` key, an ``expect`` value
            of the wrong type (``max_steps``, ``max_tokens`` and
            ``max_model_calls`` take a non-negative int, ``http_status`` an
            int, ``status`` and ``code`` a string, ``tools_called``,
            ``tools_not_called`` and ``path`` a name or a list of names), an
            ``output_matches`` that is not a valid regular expression, or
            ``scopes`` that is not a list of scope names.

    Example::

        declare(app, "greets", target="desk", input="hi", expect={"status": "completed"})
    """
    if not isinstance(name, str) or not _NAME_RE.match(name):
        raise ValueError(
            f"eval name {name!r} must be letters, digits, '_', '.' or '-' (it names the cassette file)"
        )
    if any(c.name == name for c in getattr(app, "_evals", [])):
        raise ValueError(f"duplicate eval name {name!r}")
    if not isinstance(target, str) or not target:
        raise ValueError(f"eval {name!r}: target must name a tool")
    if isinstance(input, str):
        args = {"input": input}
    elif isinstance(input, dict):
        args = dict(input)
    else:
        raise ValueError(f"eval {name!r}: input must be a dict of arguments or a string, not {type(input).__name__}")
    try:
        json.dumps(args)
    except (TypeError, ValueError) as e:
        raise ValueError(f"eval {name!r}: input must be JSON-serialisable ({e})") from None
    expect = dict(expect or {})
    unknown = sorted(k for k in expect if k not in EXPECT_KEYS)
    if unknown:
        raise ValueError(
            f"eval {name!r}: unknown expect key {unknown[0]!r}; the keys are {', '.join(EXPECT_KEYS)}"
        )
    _check_expect_types(name, expect)
    if "output_matches" in expect:
        try:
            re.compile(expect["output_matches"])
        except (re.error, TypeError) as e:
            raise ValueError(f"eval {name!r}: output_matches is not a valid regular expression ({e})") from None
    if check is not None and not callable(check):
        raise ValueError(f"eval {name!r}: check must be callable")
    # A bare string would become one scope per character.
    if isinstance(scopes, str) or not all(isinstance(s, str) for s in scopes):
        raise ValueError(f"eval {name!r}: scopes must be a list of scope names, not {scopes!r}")
    return EvalCase(name=name, target=target, args=args, expect=expect, check=check,
                    scopes=list(scopes), description=description)


def _is_count(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _is_names(value: Any, kinds: tuple = (list, tuple)) -> bool:
    return isinstance(value, str) or (isinstance(value, kinds) and all(isinstance(v, str) for v in value))


def _is_needles(value: Any) -> bool:
    def one(v: Any) -> bool:
        return isinstance(v, (str, int, float)) and not isinstance(v, bool)

    return one(value) or (isinstance(value, (list, tuple)) and all(one(v) for v in value))


def _check_expect_types(name: str, expect: dict) -> None:
    """Refuse an ``expect`` value that could only crash or never match.

    Checked at declaration for the same reason unknown keys are: a value read
    from YAML or an environment variable as ``"2"`` must not reach the
    comparison, where it raises in the middle of a run of every eval.
    """
    rules: dict[str, tuple[Callable[[Any], bool], str]] = {
        "max_steps": (lambda v: _is_count(v) and v >= 0, "a non-negative integer"),
        "max_tokens": (lambda v: _is_count(v) and v >= 0, "a non-negative integer"),
        "max_model_calls": (lambda v: _is_count(v) and v >= 0, "a non-negative integer"),
        "http_status": (lambda v: _is_count(v) and 100 <= v <= 599, "an HTTP status code (an integer)"),
        "status": (lambda v: isinstance(v, str), "a string"),
        "code": (lambda v: isinstance(v, str), "a string such as 'WC402'"),
        "tools_called": (_is_names, "a tool name or a list of tool names, in order"),
        "tools_not_called": (lambda v: _is_names(v, (list, tuple, set, frozenset)),
                             "a tool name or a list of tool names"),
        "path": (_is_names, "an agent name or a list of agent names"),
        "output_contains": (_is_needles, "a string or a list of strings"),
        "output_not_contains": (_is_needles, "a string or a list of strings"),
        "output_matches": (lambda v: isinstance(v, str), "a regular expression, as a string"),
    }
    for key, value in expect.items():
        rule = rules.get(key)
        if rule is not None and not rule[0](value):
            raise ValueError(f"eval {name!r}: expect[{key!r}] must be {rule[1]}, "
                             f"not {type(value).__name__} {value!r}")
    if "data" in expect:
        try:
            json.dumps(expect["data"])
        except (TypeError, ValueError) as e:
            raise ValueError(f"eval {name!r}: expect['data'] must be JSON-serialisable ({e})") from None


def cassette_path(directory: str | os.PathLike, name: str) -> Path:
    """Where an eval's cassette lives: ``<directory>/<name>.json``.

    Args:
        directory: The cassette directory (``evals`` by default).
        name: The eval's name.

    Returns:
        The path; it need not exist.

    Example::

        cassette_path("evals", "greets")   # Path("evals/greets.json")
    """
    return Path(directory) / f"{name}.json"


# ----------------------------------------------------------------------
# Reading a result
# ----------------------------------------------------------------------


def _is_run(body: Any) -> bool:
    return isinstance(body, dict) and "status" in body and ("run_id" in body or "flow" in body)


def _output_text(body: Any) -> str:
    value = body.get("output") if _is_run(body) else body
    return value if isinstance(value, str) else json.dumps(value, sort_keys=True, default=str)


def _data(body: Any) -> Any:
    if _is_run(body):
        return body["data"] if body.get("data") is not None else body.get("output")
    return body


def _tools_called(outcome: dict) -> list[str]:
    """Tools that ran anywhere in the request tree, in the order they started.

    Read from the trace only. There is deliberately no fallback to the outer
    run's steps: they do not hold what a nested agent, a flow step or a
    behaviour called, so a check answered from them would pass on a run that
    broke it. :func:`_trace_problem` says when the trace cannot answer.
    """
    trace = outcome.get("trace")
    if isinstance(trace, dict):
        return [s["name"].removeprefix("tool ") for s in trace.get("spans") or []
                if s.get("kind") == "tool" and s.get("code") not in _REFUSED]
    return []


def _trace_problem(outcome: dict) -> str | None:
    """Why the trace cannot answer ``tools_called`` and friends, or None.

    With no trace at all and tracing on, nothing traced ran: a plain route
    (which calls no tools and no model), or a call refused before it started.
    That is a real answer. A run envelope without a trace, a disabled tracer
    and a truncated trace are not, and the eval fails rather than passing on
    what it could not see.
    """
    trace = outcome.get("trace")
    if isinstance(trace, dict):
        dropped = trace.get("dropped_spans") or 0
        if dropped:
            return (f"the trace was truncated ({dropped} span(s) past app.tracing(max_spans=...) were "
                    f"dropped), so calls made in them cannot be seen; raise max_spans")
        return None
    if outcome.get("tracing") is False:
        return "tracing is disabled (app.tracing(enabled=False)), and these checks read the run's trace"
    if _is_run(outcome.get("body")):
        return "the run recorded no trace"
    return None


def _tokens(outcome: dict) -> int:
    usage = outcome.get("usage") or {}
    return int(usage.get("total_tokens") or 0)


def _model_calls(outcome: dict) -> int | None:
    trace = outcome.get("trace")
    return trace.get("model_calls") if isinstance(trace, dict) else None


def _same(expected: Any, actual: Any) -> bool:
    """JSON equality: ``true`` is not ``1`` and ``false`` is not ``0``, though
    ``1`` equals ``1.0``. Python's ``==`` says ``True == 1``."""
    if isinstance(expected, bool) or isinstance(actual, bool):
        return isinstance(expected, bool) and isinstance(actual, bool) and expected == actual
    if isinstance(expected, dict):
        return (isinstance(actual, dict) and expected.keys() == actual.keys()
                and all(_same(v, actual[k]) for k, v in expected.items()))
    if isinstance(expected, (list, tuple)):
        return (isinstance(actual, (list, tuple)) and len(expected) == len(actual)
                and all(_same(e, a) for e, a in zip(expected, actual)))
    return expected == actual


def _subset(expected: Any, actual: Any, at: str = "data") -> str | None:
    """Why `actual` does not contain `expected`, or None when it does."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            return f"{at} is {type(actual).__name__}, expected an object"
        for key, value in expected.items():
            if key not in actual:
                return f"{at}.{key} is missing"
            problem = _subset(value, actual[key], f"{at}.{key}")
            if problem:
                return problem
        return None
    if not _same(expected, actual):
        return f"{at} is {actual!r}, expected {expected!r}"
    return None


def _in_order(wanted: Sequence[str], seen: Sequence[str]) -> bool:
    it = iter(seen)
    return all(any(w == s for s in it) for w in wanted)


def _as_list(value: Any) -> list:
    return list(value) if isinstance(value, (list, tuple, set)) else [value]


def check_result(case: EvalCase, outcome: dict) -> list[str]:
    """Every way an eval's result falls short of what it declared.

    Args:
        case: The eval.
        outcome: What ``_core.eval_call`` returned: ``{"status", "body",
            "error", "usage", "trace", ...}``, plus ``"tracing"`` (whether
            the app traces runs) when :func:`run_evals` calls this.

    Returns:
        A list of failure messages; empty means the eval passed.
        ``tools_called``, ``tools_not_called`` and ``max_model_calls`` are
        read from the trace; when it cannot answer them (tracing disabled,
        or spans dropped at ``max_spans``) the eval fails and says so.

    Example::

        failures = check_result(case, {"status": 200, "error": None, "usage": {},
                                       "body": {"run_id": "r", "status": "completed",
                                                "output": "hello there", "steps": []}})
        assert failures == []
    """
    failures: list[str] = []
    expect = case.expect
    if outcome.get("error"):
        return [f"the call could not be made: {outcome['error']}"]
    body = outcome.get("body")
    http = outcome.get("status")
    run = _is_run(body)
    error_body = body.get("error") if isinstance(body, dict) and isinstance(body.get("error"), dict) else None

    status = body.get("status") if run else None
    code = (body.get("code") if run else None) or (error_body or {}).get("code")
    if "status" in expect:
        if status != expect["status"]:
            failures.append(f"status is {status!r}, expected {expect['status']!r}"
                            + (f" (code {code})" if code else ""))
    elif "http_status" not in expect and "code" not in expect:
        # Unless the eval says otherwise, a run must complete and a route must succeed.
        if run and status != "completed":
            why = next((s.get("error") for s in reversed(body.get("steps", [])) if s.get("error")), None)
            failures.append(f"status is {status!r}, expected 'completed'" + (f" (code {code})" if code else "")
                            + (f": {why}" if why else ""))
        elif not run and isinstance(http, int) and http >= 400:
            message = (error_body or {}).get("message", "")
            failures.append(f"the call answered {http}" + (f" ({code})" if code else "") + (f": {message}" if message else ""))
    if "http_status" in expect and http != expect["http_status"]:
        failures.append(f"http status is {http}, expected {expect['http_status']}")
    if "code" in expect and code != expect["code"]:
        failures.append(f"code is {code!r}, expected {expect['code']!r}")

    text = _output_text(body)
    for needle in _as_list(expect.get("output_contains", [])):
        if str(needle) not in text:
            failures.append(f"output does not contain {needle!r}")
    for needle in _as_list(expect.get("output_not_contains", [])):
        if str(needle) in text:
            failures.append(f"output contains {needle!r}")
    if "output_matches" in expect and not re.search(expect["output_matches"], text):
        failures.append(f"output does not match /{expect['output_matches']}/")
    if "data" in expect:
        problem = _subset(expect["data"], _data(body))
        if problem:
            failures.append(problem)

    traced = [k for k in _TRACE_KEYS if k in expect]
    problem = _trace_problem(outcome) if traced else None
    if problem:
        failures.append(f"cannot check {', '.join(traced)}: {problem}")
    elif traced:
        called = _tools_called(outcome)
        if "tools_called" in expect and not _in_order(_as_list(expect["tools_called"]), called):
            failures.append(f"tools called were {called}, expected {_as_list(expect['tools_called'])} in that order")
        for tool in _as_list(expect.get("tools_not_called", [])):
            if tool in called:
                failures.append(f"tool {tool!r} was called")
        if "max_model_calls" in expect:
            # No trace here means nothing traced ran (see _trace_problem): no model call.
            calls = _model_calls(outcome) or 0
            if calls > expect["max_model_calls"]:
                failures.append(f"made {calls} model calls, more than max_model_calls={expect['max_model_calls']}")
    if "path" in expect:
        path = body.get("path") if isinstance(body, dict) else None
        if path != _as_list(expect["path"]):
            failures.append(f"path is {path}, expected {_as_list(expect['path'])}")
    if "max_steps" in expect:
        steps = (body.get("usage") or {}).get("steps") if isinstance(body, dict) else None
        if steps is None:
            steps = len(body.get("steps", [])) if isinstance(body, dict) else 0
        if steps > expect["max_steps"]:
            failures.append(f"took {steps} steps, more than max_steps={expect['max_steps']}")
    if "max_tokens" in expect and _tokens(outcome) > expect["max_tokens"]:
        failures.append(f"spent {_tokens(outcome)} tokens, more than max_tokens={expect['max_tokens']}")

    if case.check is not None:
        try:
            verdict = case.check(body)
        except Exception as e:  # noqa: BLE001 - an eval's own check may raise anything
            failures.append(f"check raised {type(e).__name__}: {e}")
        else:
            if verdict is False:
                failures.append("check returned False")
            elif isinstance(verdict, str) and verdict:
                failures.append(f"check: {verdict}")
    return failures


# ----------------------------------------------------------------------
# Running
# ----------------------------------------------------------------------


def _coded(message: str) -> tuple[str | None, str]:
    """Split a ``"WC320: …"`` message from the core into its code and text."""
    if re.match(r"^WC\d{3}: ", message):
        return message[:5], message[7:]
    return None, message


def _base_result(case: EvalCase, mode: str) -> dict:
    return {
        "name": case.name, "target": case.target, "description": case.description,
        "ok": False, "failures": [], "mode": mode, "cassette": None,
        "status": None, "code": None, "tokens": 0, "model_calls": None,
        "duration_ms": 0, "trace_id": None, "result": None, "error": None,
    }


def _error(code: str, message: str) -> dict:
    """The ``{"code", "message", "hint"}`` of an eval that could not run."""
    from .errors import hint

    return {"code": code, "message": message, "hint": hint(code)}


def _could_not_run(result: dict, code: str, message: str) -> dict:
    result.update(ok=False, failures=[message], code=code, error=_error(code, message))
    return result


def _exists(path: Path) -> bool:
    try:
        return path.exists()
    except OSError:
        # Something is there that cannot be inspected: reading it says why.
        return True


def _read_cassette(path: Path) -> tuple[str | None, str | None]:
    """The cassette's text, or why it cannot be read (always naming the path)."""
    record = "record it again with `webcortex eval --record`"
    try:
        return path.read_text(encoding="utf-8"), None
    except FileNotFoundError:
        return None, f"no cassette at {path}; record one with `webcortex eval --record`"
    except IsADirectoryError:
        return None, f"the cassette {path} is a directory, not a file; remove it and {record}"
    except PermissionError:
        return None, f"cannot read the cassette {path}: permission denied"
    except OSError as e:
        if path.is_dir():
            return None, f"the cassette {path} is a directory, not a file; remove it and {record}"
        return None, f"cannot read the cassette {path}: {e.strerror or e}"
    except UnicodeDecodeError as e:
        return None, f"the cassette {path} is not UTF-8 text ({e.reason} at byte {e.start}); delete it and {record}"


def _write_cassette(path: Path, document: dict) -> str | None:
    """Write the cassette whole (via a temporary file), or say why it cannot be."""
    if path.is_dir():
        return f"cannot write the cassette {path}: it is a directory"
    if path.parent.exists() and not path.parent.is_dir():
        return f"cannot write the cassette {path}: {path.parent} is a file, not a directory"
    tmp = path.with_name(path.name + ".tmp")
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(tmp, path)
    except OSError as e:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        return f"cannot write the cassette {path}: {e.strerror or e}"
    return None


def _provider_failure(outcome: dict) -> tuple[str, str] | None:
    """``(code, why)`` when a model call in the run failed at the provider (no
    key, unreachable, an error answer): what a live run needs and lacked."""
    body = outcome.get("body")
    codes: list[str] = []
    if isinstance(body, dict):
        codes.append(body.get("code") or "")
        if isinstance(body.get("error"), dict):
            codes.append(body["error"].get("code") or "")
    trace = outcome.get("trace")
    if isinstance(trace, dict):
        codes.extend(trace.get("errors") or [])
    code = next((c for c in codes if c in _PROVIDER_CODES), None)
    if code is None:
        return None
    why = None
    if _is_run(body):
        why = next((s.get("error") for s in reversed(body.get("steps", [])) if s.get("error")), None)
    return code, f"{why} ({code})" if why else f"a model call failed with {code}"


def _fake_provider() -> bool:
    return os.environ.get("WEBCORTEX_FAKE_PROVIDER", "") in ("1", "true", "yes")


def _run_case(app: Any, case: EvalCase, dispatcher: Any, workers: int, *, mode: str,
              directory: str | os.PathLike) -> dict:
    from . import _core
    from .diagnostics import boot_error_code

    path = cassette_path(directory, case.name)
    result = _base_result(case, mode)
    requested = mode

    cassette_text = None
    if mode == "auto":
        mode = "replay" if _exists(path) else "live"
        result["mode"] = mode
    if mode == "replay":
        cassette_text, problem = _read_cassette(path)
        if problem:
            return _could_not_run(result, "WC320", problem)
        result["cassette"] = str(path)

    try:
        manifest_json = app.manifest_json()
        raw = _core.eval_call(manifest_json, dispatcher, workers, case.target,
                              json.dumps(case.args), list(case.scopes), mode, cassette_text)
    except (ValueError, RuntimeError) as e:
        code, message = _coded(str(e))
        if code is None:
            code = boot_error_code(message, default="WC001")
        if code == "WC320" and cassette_text is not None:
            message += f" ({path})"
        return _could_not_run(result, code, message)
    outcome = json.loads(raw)
    # Whether a missing trace means "tracing is off" or "nothing traced ran".
    outcome.setdefault("tracing", (json.loads(manifest_json).get("tracing") or {}).get("enabled", True))
    body = outcome.get("body")

    failures = check_result(case, outcome)
    error = None
    if outcome.get("error"):
        error = _error("WC320", failures[0])
    replay = outcome.get("replay")
    provider = _provider_failure(outcome)
    if replay and isinstance(body, dict) and body.get("code") == "WC319":
        # The cassette could not answer this run, so nothing else about the
        # result means anything: report the cause, not its consequences.
        why = next((s.get("error") for s in reversed(body.get("steps", [])) if s.get("error")), "")
        failures = [f"{why or 'WC319: the cassette could not answer this run'}; if the change is intended, "
                    f"record again with `webcortex eval --record`"]
        error = _error("WC319", failures[0])
    elif replay and replay.get("unused"):
        failures.append(
            f"the run made fewer model calls than the cassette holds ({replay['unused']} of "
            f"{replay['recorded']} unused); if that is intended, record again with `webcortex eval --record`"
        )
    elif provider and mode == "record":
        # Nothing the model said was obtained, so a cassette would replay a
        # run that never happened (and fail with WC319 next time).
        failures = [f"not recorded: {provider[1]}; {path} was not written. Make the model reachable "
                    f"(`webcortex doctor` checks every model) and record again"]
        error = _error(provider[0], failures[0])
    elif provider and requested == "auto" and mode == "live":
        failures = [f"no cassette at {path}, so this eval asked the model live, and {provider[1]}. "
                    f"Set the provider's key and run `webcortex eval --record` once to save the model's "
                    f"answers (commit {Path(directory)}/); after that `webcortex eval` replays them with no "
                    f"key and no tokens"]
        error = _error(provider[0], failures[0])
    elif failures and requested == "auto" and mode == "live" and _fake_provider():
        failures.append(f"there is no cassette at {path}, so the offline fake provider answered "
                        f"(WEBCORTEX_FAKE_PROVIDER), and it follows only tool:/handoff: commands; an eval "
                        f"written for a real model needs a recording: `webcortex eval --record` with a "
                        f"provider key")

    if mode == "record" and not outcome.get("error") and not provider:
        cassette = outcome.get("cassette") or {"version": 1, "calls": []}
        from . import __version__

        document = {"version": cassette["version"], "eval": case.name, "target": case.target,
                    "recorded_with": f"webcortex {__version__}", "calls": cassette["calls"]}
        problem = _write_cassette(path, document)
        if problem:
            failures.append(problem)
            error = error or _error("WC320", problem)
        else:
            result["cassette"] = str(path)

    error_body = body.get("error") if isinstance(body, dict) and isinstance(body.get("error"), dict) else {}
    trace = outcome.get("trace") or {}
    code = (body.get("code") if isinstance(body, dict) else None) or error_body.get("code")
    result.update(
        ok=not failures,
        failures=failures,
        status=body.get("status") if _is_run(body) else outcome.get("status"),
        code=code or (error or {}).get("code"),
        tokens=_tokens(outcome),
        model_calls=_model_calls(outcome),
        duration_ms=outcome.get("duration_ms", 0),
        trace_id=trace.get("trace_id"),
        result=body,
        error=error,
    )
    if replay:
        result["replay"] = replay
    return result


def run_evals(app: Any, *, mode: str = "auto", directory: str | os.PathLike = "evals",
              only: Sequence[str] | None = None) -> dict:
    """Run an app's declared evals in-process and report on each.

    Every eval gets a freshly built app (so in-memory state such as sessions,
    and an in-memory database, start clean) and calls its target as the
    principal ``eval`` with the eval's scopes. No port is bound.

    Args:
        app: The :class:`webcortex.WebCortex` app.
        mode: Where model responses come from.

            * ``"auto"`` — replay the eval's cassette when it exists, otherwise
              ask the providers (the fake one under ``WEBCORTEX_FAKE_PROVIDER=1``).
            * ``"replay"`` — replay; an eval with no cassette fails (``WC320``).
              Use this in CI so a missing recording cannot pass as a live run.
            * ``"live"`` — always ask the providers; cassettes are ignored.
            * ``"record"`` — ask the providers and write each eval's cassette.
        directory: Where cassettes live (``evals`` by default).
        only: Names of the evals to run; ``None`` runs them all.

    Returns:
        ``{"ok", "total", "passed", "failed", "tokens", "mode", "directory",
        "results": [...]}``. Each result has ``name``, ``target``,
        ``description``, ``mode`` (the one actually used), ``ok``,
        ``failures`` (messages), ``status``, ``code``, ``tokens`` (every model
        call in the request tree, as recorded when replayed), ``model_calls``,
        ``duration_ms``, ``trace_id``, ``cassette`` (the file used or written),
        ``result`` (the tool's response body) and ``error``: ``None``, or
        ``{"code", "message", "hint"}`` for an eval that could not be run
        or trusted (no cassette or an unreadable one, ``WC320``; a stale one,
        ``WC319``; a model that could not be reached, ``WC301``-``WC305``).
        An exception while running or checking one eval fails that eval
        (``WC320``) and the others still run.

    Raises:
        ValueError: for an unknown ``mode``, or a name in ``only`` that is not
            a declared eval.

    Example::

        report = run_evals(app, mode="replay")
        for r in report["results"]:
            print(r["name"], "ok" if r["ok"] else r["failures"])
    """
    if mode not in MODES:
        raise ValueError(f"run_evals(): mode must be one of {', '.join(MODES)}, not {mode!r}")
    cases: list[EvalCase] = list(getattr(app, "_evals", []))
    if only:
        known = {c.name for c in cases}
        missing = [n for n in only if n not in known]
        if missing:
            raise ValueError(f"no eval named {missing[0]!r}; declared: {', '.join(sorted(known)) or 'none'}")
        cases = [c for c in cases if c.name in set(only)]

    from ._bridge import Dispatcher, default_worker_count

    workers = app.workers or default_worker_count()
    results: list[dict] = []
    if cases:
        dispatcher = Dispatcher(app._handlers, workers)
        try:
            for case in cases:
                try:
                    result = _run_case(app, case, dispatcher, workers, mode=mode, directory=directory)
                except Exception as e:  # noqa: BLE001 - one broken eval must not lose the report
                    result = _could_not_run(_base_result(case, mode), "WC320",
                                            f"the eval could not be run: {type(e).__name__}: {e}")
                results.append(result)
        finally:
            dispatcher.shutdown()
    passed = sum(1 for r in results if r["ok"])
    return {
        "ok": passed == len(results),
        "total": len(results),
        "passed": passed,
        "failed": len(results) - passed,
        "tokens": sum(r["tokens"] for r in results),
        "mode": mode,
        "directory": str(directory),
        "results": results,
    }


def render_report(report: dict) -> str:
    """The text ``webcortex eval`` prints for a report.

    Args:
        report: What :func:`run_evals` returned.

    Returns:
        One line per eval (pass or fail, the mode used, tokens, duration), the
        failure messages of each failed one, and a summary line.

    Example::

        print(render_report(run_evals(app)))
    """
    if not report["results"]:
        return "  no evals declared: add app.eval(name, target=..., input=..., expect={...})"
    out = []
    for r in report["results"]:
        mark = "ok  " if r["ok"] else "FAIL"
        calls = f"  {r['model_calls']} model call(s)" if r.get("model_calls") is not None else ""
        out.append(f"  {mark} {r['name']:<32} {r['mode']:<6} → {r['target']}  "
                   f"{r['tokens']} tok{calls}  {r['duration_ms']}ms")
        for failure in r["failures"]:
            out.append(f"         - {failure}")
        if r["mode"] == "record" and r.get("cassette"):
            out.append(f"         recorded {r['cassette']}")
    replayed = sum(1 for r in report["results"] if r["mode"] == "replay")
    out.append(f"  {report['passed']}/{report['total']} passed, {replayed} replayed from cassettes, "
               f"{report['tokens']} tokens")
    return "\n".join(out)
