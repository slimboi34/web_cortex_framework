"""Property tests for the Python layer (and the runtime behind it), from the
machine-driven testing of 2.9.0 that 2.9.1 fixes.

Fast and deterministic by default (derandomized Hypothesis, small example
counts, short-lived servers on free ports, FakeProvider only). Set
``WEBCORTEX_PROPS_PROFILE=campaign`` for a long randomized run.

The tests at the end were ``xfail(strict=True)`` on 2.9.0, one per defect the
testing found (F1-F9 in the 2.9.1 CHANGELOG); each passes since 2.9.1 except
the protocol-error one, a documented limit of hyper (docs/error-codes.md).
Needs ``hypothesis`` (CI installs it); without it this module is skipped.
"""
from __future__ import annotations

import base64
import dataclasses
import datetime as dt
import decimal
import enum
import hashlib
import hmac
import json
import math
import os
import socket
import subprocess
import sys
import textwrap
import time
import typing
import urllib.error
import urllib.request
import uuid
from contextlib import contextmanager
from pathlib import Path
from typing import Literal, Optional, TypedDict

import pytest

pytest.importorskip("hypothesis")
from hypothesis import HealthCheck, assume, event, example, given, settings, strategies as st  # noqa: E402

from webcortex import WebCortex  # noqa: E402
from webcortex._bridge import MAX_JSON_DEPTH, InvalidJSON, Request  # noqa: E402
from webcortex.schema import coerce  # noqa: E402

settings.register_profile("fast", max_examples=150, derandomize=True, deadline=None, database=None,
                          suppress_health_check=list(HealthCheck))
settings.register_profile("campaign", max_examples=20000, deadline=None,
                          suppress_health_check=list(HealthCheck))
if os.environ.get("WEBCORTEX_PROPS_DB"):
    from hypothesis.database import DirectoryBasedExampleDatabase
    settings.register_profile("hypofuzz", deadline=None, suppress_health_check=[HealthCheck.too_slow, HealthCheck.filter_too_much],
                              database=DirectoryBasedExampleDatabase(os.environ["WEBCORTEX_PROPS_DB"]))
settings.load_profile(os.environ.get("WEBCORTEX_PROPS_PROFILE", "fast"))


@pytest.fixture(scope="module", autouse=True)
def _fake_provider():
    """Every run here is on the FakeProvider. Set for this module only: a bare
    ``os.environ`` write would leak into every test file that runs after it."""
    with pytest.MonkeyPatch.context() as mp:
        mp.setenv("WEBCORTEX_FAKE_PROVIDER", "1")
        yield


# ====================================================================== coerce


class Colour(enum.Enum):
    RED = "red"
    BLUE = "blue"


class Level(enum.IntEnum):
    LOW = 1
    HIGH = 2


@dataclasses.dataclass
class Item:
    name: str
    qty: int = 1


class Point(TypedDict):
    x: int
    y: float


ANNOTATIONS = [
    int, float, bool, str, dt.date, dt.datetime, dt.time, Colour, Level, decimal.Decimal, uuid.UUID, bytes,
    list[int], list[float], set[int], frozenset[str], tuple[int, ...], tuple[int, str], dict[str, int],
    Optional[int], Optional[list[dt.date]], Literal[1, "a", 2.5], Item, Point, list[Item],
    typing.Annotated[int, "meta"], typing.Any,
]

json_scalars = st.one_of(
    st.none(), st.booleans(), st.integers(), st.floats(allow_nan=False, allow_infinity=False), st.text(),
    st.sampled_from([10**400, -(10**400), 2**53 + 1, 2**63, 2**64 + 1, 10**4000]),
    st.sampled_from(["1e400", "-1e400", "1e-400", "nan", "inf", " 7 ", "1_000", "٣", "0x10", "+5",
                     "2020-01-02", "2020-01-02T03:04:05+01:00", "12:30", "red", "RED", "1", "true", "off",
                     "YWJj", "not base64!", str(uuid.UUID(int=5)), "9" * 5000]),
)
json_values = st.recursive(json_scalars, lambda c: st.one_of(
    st.lists(c, max_size=4), st.dictionaries(st.sampled_from(["name", "qty", "x", "y", "z"]), c, max_size=3)),
    max_leaves=8)


@given(json_values, st.sampled_from(ANNOTATIONS))
@example(10**400, float)
@example([10**400], list[float])
def test_coerce_never_raises(value, annotation):
    """F3: through 2.9.0 coerce(10**400, float) raised OverflowError, so any
    client's request to a float parameter answered 500 WC002."""
    coerce(value, annotation)


def _has_huge_int(v) -> bool:
    if isinstance(v, bool):
        return False
    if isinstance(v, int):
        return abs(v) > 10**300
    if isinstance(v, list):
        return any(_has_huge_int(x) for x in v)
    if isinstance(v, dict):
        return any(_has_huge_int(x) for x in v.values())
    return False


@given(json_values.filter(lambda v: not _has_huge_int(v)), st.sampled_from(ANNOTATIONS))
def test_coerce_never_raises_below_float_range(value, annotation):
    """The same property with the known overflow class taken out, so any *other*
    exception still fails the suite."""
    try:
        coerce(value, annotation)
    except ValueError as e:
        # str(int) past sys.int_info.str_digits_check_threshold: not reachable
        # from a JSON body (json.loads has the same limit), reported separately.
        assert "integer string conversion" in str(e)


@given(st.integers())
@example(2**53 + 1)
def test_coerce_float_from_int_loses_nothing(n):
    """'Conversions never lose information' (schema.coerce docstring): an int
    either becomes a float equal to it, or passes through unchanged."""
    out = coerce(n, float)
    assert out is n or (isinstance(out, float) and math.isfinite(out) and int(out) == n)


@given(st.integers(-(10**200), 10**200))
def test_coerce_int_from_its_text(n):
    assert coerce(str(n), int) == n
    assert coerce(f"  {n}\n", int) == n


@given(st.floats(allow_nan=False, allow_infinity=False))
def test_coerce_float_from_its_text(x):
    out = coerce(repr(x), float)
    assert out == x and (math.copysign(1, out) == math.copysign(1, x))


@given(st.floats())
def test_coerce_int_from_float_only_when_integral(x):
    out = coerce(x, int)
    if math.isfinite(x) and x.is_integer():
        assert isinstance(out, int) and out == x
    else:
        assert out is x  # 3.7 is not an int: passed through


@given(json_values, st.sampled_from([int, float, bool, str, dt.date, Colour, Level, decimal.Decimal, uuid.UUID]))
def test_coerce_is_idempotent_on_scalars(value, annotation):
    assume(not _has_huge_int(value))
    try:
        once = coerce(value, annotation)
    except ValueError:
        return  # >4300-digit int text, see above
    twice = coerce(once, annotation)
    assert twice == once or (isinstance(once, float) and math.isnan(once))


@given(st.dates())
def test_coerce_date_roundtrip(d):
    assert coerce(d.isoformat(), dt.date) == d
    assert coerce(d, dt.date) is d


@given(st.datetimes(timezones=st.none() | st.builds(lambda m: dt.timezone(dt.timedelta(minutes=m)), st.integers(-1439, 1439))))
def test_coerce_datetime_roundtrip(t):
    assert coerce(t.isoformat(), dt.datetime) == t


@given(st.sampled_from(list(Colour) + list(Level)))
def test_coerce_enum_by_value_and_name(member):
    cls = type(member)
    assert coerce(member.value, cls) is member
    assert coerce(member.name, cls) is member
    assert coerce(str(member.value), cls) is member


@given(st.text())
def test_coerce_bool_only_from_the_documented_words(text):
    out = coerce(text, bool)
    word = text.strip().lower()
    if word in {"1", "true", "yes", "on"}:
        assert out is True
    elif word in {"0", "false", "no", "off"}:
        assert out is False
    else:
        assert out == text


@given(st.lists(st.integers(-(10**30), 10**30)))
def test_coerce_list_is_elementwise(xs):
    assert coerce([str(x) for x in xs], list[int]) == xs


@given(st.text(), st.integers(-(10**9), 10**9))
def test_coerce_dataclass_roundtrip(name, qty):
    out = coerce({"name": name, "qty": str(qty)}, Item)
    assert out == Item(name=name, qty=qty)
    raw = {"name": name, "qty": qty, "other": 1}
    assert coerce(raw, Item) is raw  # undeclared key: passed through


@given(st.binary())
def test_coerce_bytes_from_base64(b):
    assert coerce(base64.b64encode(b).decode(), bytes) == b


# =============================================================== Request.json


def _nest(depth: int, kinds: list[str]) -> str:
    opens, closes = [], []
    for i in range(depth):
        k = kinds[i % len(kinds)]
        if k == "[":
            opens.append("[")
            closes.append("]")
        else:
            opens.append('{"k":')
            closes.append("}")
    return "".join(opens) + "1" + "".join(reversed(closes))


@given(st.integers(1, MAX_JSON_DEPTH + 40), st.lists(st.sampled_from(["[", "{"]), min_size=1, max_size=5),
       st.booleans())
@example(MAX_JSON_DEPTH, ["["], False)
@example(MAX_JSON_DEPTH + 1, ["["], False)
@example(MAX_JSON_DEPTH + 1, ["{"], True)
def test_request_json_depth_limit_is_exact(depth, kinds, as_str):
    text = _nest(depth, kinds)
    raw = text if as_str else text.encode()
    req = Request({"method": "POST", "path": "/", "path_params": {}, "query": {}, "headers": {}, "body": raw,
                   "scopes": []})
    if depth <= MAX_JSON_DEPTH:
        assert req.json() == json.loads(text)
    else:
        with pytest.raises(InvalidJSON) as e:
            req.json()
        assert e.value.status == 400 and e.value.code == "WC205"


@given(st.integers(0, 3000), st.integers(1, 40))
def test_request_json_brackets_inside_strings_do_not_count(n_brackets, depth):
    """Lots of `[` in string data forces the slow path; depth decides, not the count."""
    text = _nest(depth, ["["]).replace("1", json.dumps("[" * n_brackets + "{" * n_brackets), 1)
    req = Request({"method": "POST", "path": "/", "path_params": {}, "query": {}, "headers": {},
                   "body": text.encode(), "scopes": []})
    assert req.json() == json.loads(text)


@given(st.binary(max_size=64))
def test_request_json_never_raises_anything_but_invalidjson(raw):
    req = Request({"method": "POST", "path": "/", "path_params": {}, "query": {}, "headers": {}, "body": raw,
                   "scopes": []})
    try:
        req.json()
    except InvalidJSON as e:
        assert e.status == 400 and e.code == "WC205"


# ======================================================== eval fingerprints

FP_BASE = dict(system="You triage.", model="default", temperature=None, max_tokens=16000, reasoning=None,
               tool_desc="Look up a ticket.", tool_param="int", output=False, options=None, alias="claude-opus-5",
               ctx_value="v1", provider_opt=None)
# option -> (changed value, does the fingerprint have to change?)
FP_CHANGES = {
    "system": ("You triage carefully.", True),
    "model": ("fast", True),
    "temperature": (0.9, True),
    "max_tokens": (1000, True),
    "reasoning": ("high", True),
    "tool_desc": ("Look up a ticket by id.", True),
    "tool_param": ("str", True),
    "options": ({"top_k": 5}, True),
    "alias": ("claude-sonnet-5", True),
    "ctx_value": ("v2", True),
    "provider_opt": ({"top_k": 3}, True),
}


def _fp_app(cfg: dict, *, extra_route: bool = False, reorder: bool = False) -> WebCortex:
    app = WebCortex("fp")
    app.models(default=cfg["alias"], fast="claude-haiku-5")
    if cfg["provider_opt"]:
        app.request_options("anthropic", **cfg["provider_opt"])
    app.context("policy", data={"v": cfg["ctx_value"]}, description="Policy.")

    def declare_tool():
        if cfg["tool_param"] == "int":
            def lookup(ticket: int) -> dict:
                return {"ticket": ticket}
        else:
            def lookup(ticket: str) -> dict:
                return {"ticket": ticket}
        lookup.__doc__ = cfg["tool_desc"]
        app.get("/tickets/{ticket}", tool=True, tool_name="lookup", summary=cfg["tool_desc"])(lookup)

    def declare_noise():
        if extra_route:
            app.get("/noise")(lambda: {"n": 1})
            app.agent("bystander", system="Unrelated.")

    order = [declare_noise, declare_tool] if reorder else [declare_tool, declare_noise]
    for f in order:
        f()

    @dataclasses.dataclass
    class Out:
        answer: str

    kw = {}
    if cfg["temperature"] is not None:
        kw["temperature"] = cfg["temperature"]
    if cfg["reasoning"]:
        kw["reasoning"] = cfg["reasoning"]
    if cfg["options"]:
        kw["options"] = cfg["options"]
    app.agent("desk", system=cfg["system"], model=cfg["model"], tools=["lookup"], context=["policy"],
              max_tokens=cfg["max_tokens"], output=Out if cfg["output"] else None, **kw)
    app.eval("e1", target="desk", input='tool:lookup {"ticket": 3}' if not cfg["output"] else
             'json:{"answer": "x"}')
    return app


@pytest.fixture(scope="module")
def fp_cassettes(tmp_path_factory, _fake_provider):
    d = tmp_path_factory.mktemp("cassettes")
    report = _fp_app(FP_BASE).run_evals(mode="record", directory=str(d))
    assert report["ok"], report["results"]
    return d


def _replay(cfg, d, **kw) -> dict:
    report = _fp_app(cfg, **kw).run_evals(mode="replay", directory=str(d))
    return report["results"][0]


def test_fingerprint_stable_under_noop_changes(fp_cassettes):
    for kw in ({}, {"extra_route": True}, {"reorder": True}, {"extra_route": True, "reorder": True}):
        r = _replay(FP_BASE, fp_cassettes, **kw)
        assert r["ok"] and not r["error"], (kw, r["failures"], r["error"])


@pytest.mark.parametrize("option", sorted(FP_CHANGES))
def test_fingerprint_changes_with_every_option_that_matters(fp_cassettes, option):
    value, must_change = FP_CHANGES[option]
    cfg = {**FP_BASE, option: value}
    r = _replay(cfg, fp_cassettes)
    stale = bool(r["error"]) and r["error"]["code"] == "WC319"
    assert stale == must_change, (option, r["failures"], r["error"])


# ================================================================ live server

ADMIN = "props-admin-key-0123456789abcdefABCDEF"
SECRET = "props-jwt-secret-0123456789abcdefABCDEF-xyz"


_PORTS_USED = [0]


def _free_port() -> int:
    if os.environ.get("WEBCORTEX_PROPS_PORT"):  # a fixed base, one port per server
        _PORTS_USED[0] += 1
        return int(os.environ["WEBCORTEX_PROPS_PORT"]) + _PORTS_USED[0] - 1
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@contextmanager
def serve(tmp: Path, source: str, env: dict | None = None, port: int | None = None):
    port = port or _free_port()
    (tmp / "api.py").write_text(textwrap.dedent(source).replace("PORT", str(port)))
    log = tmp / "server.log"
    full_env = {**os.environ, "WEBCORTEX_FAKE_PROVIDER": "1", "WEBCORTEX_LOG": "info",
                "PROPS_ADMIN": ADMIN, "PROPS_JWT": SECRET, **(env or {})}
    full_env.pop("ANTHROPIC_API_KEY", None)
    with log.open("w") as f:
        proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"], cwd=tmp,
                                stdout=f, stderr=subprocess.STDOUT, env=full_env)
    base = f"http://127.0.0.1:{port}"
    try:
        deadline = time.time() + 30
        while True:
            if proc.poll() is not None:
                pytest.fail(f"server exited:\n{log.read_text()}")
            try:
                urllib.request.urlopen(base + "/_webcortex/health", timeout=1).read()
                break
            except Exception:
                if time.time() > deadline:
                    pytest.fail(f"server not ready:\n{log.read_text()}")
                time.sleep(0.1)
        yield base, log
    finally:
        proc.kill()
        proc.wait(10)


JWT_APP = """
from webcortex import WebCortex
app = WebCortex("jwtprops", port=PORT)
app.api_key("PROPS_ADMIN", id="ops", scopes=["secret"])
app.jwt(secret_env="PROPS_JWT", audience="aud-1", issuer="iss-1", leeway_secs=5)

@app.get("/secret", scopes=["secret"])
def secret():
    return {"canary": "CANARY"}

if __name__ == "__main__":
    app.run()
"""


def _b64u(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _mint(claims: dict, *, secret: str = SECRET, alg: str = "HS256") -> str:
    head = _b64u(json.dumps({"alg": alg, "typ": "JWT"}).encode())
    body = _b64u(json.dumps(claims).encode())
    mac = {"HS256": hashlib.sha256, "HS384": hashlib.sha384}[alg]
    return f"{head}.{body}." + _b64u(hmac.new(secret.encode(), f"{head}.{body}".encode(), mac).digest())


def _get(url: str, headers: dict) -> tuple[int, bytes]:
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=10) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


@pytest.fixture(scope="module")
def jwt_server(tmp_path_factory):
    with serve(tmp_path_factory.mktemp("jwt"), JWT_APP) as (base, log):
        yield base


NOW = int(time.time())
claim_values = {
    "sub": st.one_of(st.none(), st.sampled_from(["u1", "", "ops", "watcher:x", "watcher:", "Ops", " ops", "watcher",
                                                 "jwt-subject-unset", "anonymous", "eval"]), st.text(max_size=8)),
    "aud": st.one_of(st.none(), st.sampled_from(["aud-1", "aud-2", "", "AUD-1"]),
                     st.lists(st.sampled_from(["aud-1", "aud-2", "x"]), max_size=3)),
    "iss": st.one_of(st.none(), st.sampled_from(["iss-1", "iss-2", "", "ISS-1", "iss-1 "])),
    "exp": st.one_of(st.none(), st.sampled_from([NOW - 3600, NOW - 6, NOW + 2, NOW + 3600, NOW + 10**9, 0, -1,
                                                 "soon", float(NOW + 3600), None])),
    "nbf": st.one_of(st.none(), st.sampled_from([NOW - 60, NOW + 3600, NOW + 2, "x"])),
    "scope": st.one_of(st.none(), st.sampled_from(["secret", "read secret", "SECRET", "secrets", "", "*"])),
    "scopes": st.one_of(st.none(), st.lists(st.sampled_from(["secret", "read", "*"]), max_size=2)),
}


def _expected_accept(c: dict, alg: str, good_key: bool) -> bool:
    """What the docs (security.md, app.jwt, CHANGELOG 2.9.0) say is accepted."""
    if not good_key or alg != "HS256":
        return False
    exp = c.get("exp")
    if not isinstance(exp, (int, float)) or isinstance(exp, bool) or exp + 5 <= NOW:
        return False  # exp is required and validated, leeway 5 s
    nbf = c.get("nbf")
    if nbf is not None and (not isinstance(nbf, (int, float)) or nbf - 5 > NOW):
        return False
    aud = c.get("aud")
    if aud is None or not (aud == "aud-1" or (isinstance(aud, list) and "aud-1" in aud)):
        return False  # aud required when configured
    if c.get("iss") != "iss-1":
        return False  # iss required when configured
    sub = c.get("sub")
    if sub is not None and not isinstance(sub, str):
        return False
    if sub == "ops" or (isinstance(sub, str) and sub.startswith("watcher:")):
        return False  # an API key's id, or a watcher's: refused (2.9.0)
    scopes = set((c.get("scope") or "").split()) | set(c.get("scopes") or [])
    return "secret" in scopes or "*" in scopes


VALID_BASE = {"sub": "u1", "aud": "aud-1", "iss": "iss-1", "exp": NOW + 3600, "scope": "secret"}


@settings(max_examples=int(os.environ.get("WEBCORTEX_PROPS_JWT_EXAMPLES", "120")))
@given(st.one_of(st.fixed_dictionaries({}, optional=claim_values),
                 st.fixed_dictionaries({}, optional=claim_values).map(lambda c: {**VALID_BASE, **c})),
       st.sampled_from(["HS256", "HS256", "HS256", "HS384"]), st.booleans())
def test_jwt_never_accepts_what_the_docs_refuse(jwt_server, claims, alg, good_key):
    claims = {k: v for k, v in claims.items() if v is not None}
    token = _mint(claims, secret=SECRET if good_key else SECRET + "x", alg=alg)
    status, body = _get(jwt_server + "/secret", {"authorization": f"Bearer {token}"})
    accepted = b"CANARY" in body
    expected = _expected_accept(claims, alg, good_key)
    event(f"docs say accept={expected}, server accepted={accepted}")
    if expected:
        # Liveness: a token the docs accept must work (unless it expires mid-test).
        assert accepted or claims.get("exp", 0) - time.time() < 10, (claims, status, body)
    else:
        assert not accepted, (claims, alg, good_key, status, body)
        assert status in (401, 403), (status, body)
        err = json.loads(body)["error"]
        assert err["code"].startswith("WC6") and err["hint"]


# ================================================================ OTLP headers

OTLP_APP = """
from webcortex import WebCortex
app = WebCortex("otlpprops", port=PORT)
app.tracing(keep=10)
app.agent("a", system="s")
if __name__ == "__main__":
    app.run()
"""


def _reference_headers(raw: str) -> tuple[list[tuple[str, str]], list[int]]:
    """OTEL_EXPORTER_OTLP_HEADERS per the OpenTelemetry spec, as 2.9.0 documents it:
    comma-separated k=v, each trimmed then percent-decoded; bad entries skipped."""
    from urllib.parse import unquote_to_bytes
    token = set("!#$%&'*+-.^_`|~0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ")
    out, skipped = [], []
    for i, entry in enumerate(raw.split(","), 1):
        if not entry.strip():
            continue
        if "=" not in entry:
            skipped.append(i)
            continue
        k, v = entry.split("=", 1)
        try:
            k = unquote_to_bytes(k.strip()).decode("utf-8")
            v = unquote_to_bytes(v.strip()).decode("utf-8")
        except UnicodeDecodeError:
            skipped.append(i)
            continue
        if not k or any(ch not in token for ch in k) or any(
                not ((b >= 32 and b != 127) or b == 9) for b in v.encode()):
            skipped.append(i)
            continue
        out.append((k.lower(), v))
    return out, skipped


def _sink(port: int):
    import http.server
    import threading
    got = []

    class H(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            n = int(self.headers.get("content-length") or 0)
            self.rfile.read(n)
            got.append([(k.lower(), v) for k, v in self.headers.raw_items()])
            self.send_response(200)
            self.send_header("content-length", "0")
            self.end_headers()

        def log_message(self, *a):
            pass

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, got


OTLP_FIXED = (" Authorization = Bearer%20abc%3D%3D , x-k=%E2%9C%93 ,,"
              "noequals, bad%FF=1, =novalue, x-pct=%ZZ%2, x-sp=a%20b , X-Upper=V, x-eq=a=b, sp ace=1,"
              "x-ctl=%0A, x-tab=a%09b")


def test_otlp_headers_are_trimmed_decoded_and_bad_entries_skipped(tmp_path):
    sink_port = _free_port()
    srv, got = _sink(sink_port)
    try:
        env = {"OTEL_EXPORTER_OTLP_ENDPOINT": f"http://127.0.0.1:{sink_port}", "OTEL_EXPORTER_OTLP_HEADERS": OTLP_FIXED}
        with serve(tmp_path, OTLP_APP, env=env) as (base, log):
            req = urllib.request.Request(base + "/agents/a", data=b'{"input": "hi"}', method="POST",
                                         headers={"content-type": "application/json"})
            urllib.request.urlopen(req, timeout=10).read()
            deadline = time.time() + 10
            while not got and time.time() < deadline:
                time.sleep(0.2)
            text = log.read_text()
    finally:
        srv.shutdown()
    assert got, "no export request arrived"
    expected, skipped = _reference_headers(OTLP_FIXED)
    sent = got[0]
    for k, v in expected:
        # http.server reads header bytes as latin-1
        assert (k, v.encode("utf-8").decode("latin-1")) in sent, (k, v, sent)
    standard = {"content-type", "content-length", "host", "accept", "accept-encoding", "user-agent", "connection"}
    extra = [(k, v) for k, v in sent if k not in standard]
    assert sorted(extra) == sorted((k, v.encode("utf-8").decode("latin-1")) for k, v in expected), extra
    for n in skipped:
        assert f"entry {n} " in text, (n, text[-2000:])
    for secret in ("abc==", "%FF", "novalue"):
        assert secret not in text


# ============================================================ the worker pool

from hypothesis.stateful import RuleBasedStateMachine, invariant, precondition, rule  # noqa: E402

from webcortex._bridge import _Pool, behaviour_limit  # noqa: E402


class PoolMachine(RuleBasedStateMachine):
    """`_Pool(size, limit=size)` (the behaviour pool): every admitted task runs at
    once on a thread of its own, the (size+1)-th is refused, and the count of
    admitted tasks always equals tasks submitted and not yet finished."""

    SIZE = 3

    def __init__(self):
        super().__init__()
        self.pool = _Pool(self.SIZE, "props", limit=self.SIZE)
        self.running: list[tuple] = []  # (started, release, done)
        self.closed = False

    @rule(fail=st.booleans())
    def submit(self, fail):
        import threading
        started, release, done = threading.Event(), threading.Event(), threading.Event()

        def task():
            started.set()
            release.wait(10)
            done.set()
            if fail:
                raise RuntimeError("task failure must not kill the pool")

        ok = self.pool.try_submit(task)
        if self.closed or len(self.running) >= self.SIZE:
            assert not ok, "admitted past the limit, or after shutdown"
            return
        assert ok, f"refused with {len(self.running)} of {self.SIZE} running"
        assert started.wait(5), "an admitted task did not start: it was queued behind busy threads"
        self.running.append((started, release, done))

    @precondition(lambda self: self.running)
    @rule(i=st.integers(0, 10))
    def finish(self, i):
        started, release, done = self.running.pop(i % len(self.running))
        release.set()
        assert done.wait(5)
        deadline = time.time() + 5
        while self.pool.admitted != len(self.running) and time.time() < deadline:
            time.sleep(0.005)

    @rule()
    def shutdown(self):
        self.pool.shutdown()
        self.closed = True

    @invariant()
    def accounting(self):
        assert self.pool.admitted >= len(self.running)

    def teardown(self):
        for _, release, _ in self.running:
            release.set()
        self.pool.shutdown()


PoolMachine.TestCase.settings = settings(max_examples=40, stateful_step_count=25, deadline=None,
                                         derandomize=True, database=None)
test_behaviour_pool_never_queues_admitted_work = PoolMachine.TestCase


@given(st.one_of(st.none(), st.text(max_size=6), st.integers(-5, 10**6).map(str)), st.integers(1, 64))
def test_behaviour_limit_env(monkeypatch_value, workers):
    old = os.environ.get("WEBCORTEX_MAX_BEHAVIOURS")
    try:
        if monkeypatch_value is None:
            os.environ.pop("WEBCORTEX_MAX_BEHAVIOURS", None)
        else:
            os.environ["WEBCORTEX_MAX_BEHAVIOURS"] = monkeypatch_value
        got = behaviour_limit(workers)
        try:
            v = int(monkeypatch_value or "")
        except ValueError:
            v = 0
        assert got == (v if v > 0 else max(32, workers * 8))
    finally:
        if old is None:
            os.environ.pop("WEBCORTEX_MAX_BEHAVIOURS", None)
        else:
            os.environ["WEBCORTEX_MAX_BEHAVIOURS"] = old



# ======================================================= the 2.9.0 defects
# One per finding of the 2.9.0 testing (F1-F9 in the 2.9.1 CHANGELOG). Each was
# xfail(strict=True) against 2.9.0 and passes since 2.9.1, except F5, a
# documented limit of hyper that stays xfail so a hyper that grows a hook for
# its own error responses turns it into a reminder.


def _typed_run(schema: dict, answer: dict, *, via_tool: bool = False) -> tuple[dict, float]:
    from webcortex import _core
    from webcortex._bridge import Dispatcher
    app = WebCortex("defects")
    app.agent("a", output=schema, max_steps=4)
    text = ("tool:final_answer " if via_tool else "json:") + json.dumps(answer)
    started = time.time()
    raw = _core.eval_call(app.manifest_json(), Dispatcher(app._handlers, 1), 1, "a",
                          json.dumps({"input": text}), [], "live", None)
    return json.loads(raw)["body"], time.time() - started


def test_typed_answer_keeps_integers_beyond_u64_exactly():
    """F4: an integer past u64 is parsed as a double, so it is refused (WC318)
    rather than handed back rounded."""
    body, _ = _typed_run({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]},
                         {"n": 2**64 + 1})
    assert body.get("status") != "completed" or body["data"]["n"] == 2**64 + 1


def test_checking_a_recursive_union_answer_is_not_exponential():
    """F1: 2.9.0 checked every alternative in full, ~2^depth work."""
    defs = {
        "Lit": {"type": "object", "properties": {"kind": {"const": "lit"}, "value": {"type": "integer"}},
                "required": ["kind", "value"], "additionalProperties": False},
        "Add": {"type": "object", "properties": {"kind": {"const": "add"}, "left": {"$ref": "#/$defs/E"},
                                                  "right": {"$ref": "#/$defs/E"}},
                "required": ["kind", "left", "right"], "additionalProperties": False},
        "Sub": {"type": "object", "properties": {"kind": {"const": "sub"}, "left": {"$ref": "#/$defs/E"},
                                                  "right": {"$ref": "#/$defs/E"}},
                "required": ["kind", "left", "right"], "additionalProperties": False},
        "E": {"anyOf": [{"$ref": "#/$defs/Add"}, {"$ref": "#/$defs/Sub"}, {"$ref": "#/$defs/Lit"}]},
    }
    schema = {"type": "object", "properties": {"expr": {"$ref": "#/$defs/E"}}, "required": ["expr"],
              "additionalProperties": False, "$defs": defs}
    e = {"kind": "lit", "value": 0}
    for _ in range(16):
        e = {"kind": "add", "left": e, "right": {"kind": "lit", "value": 1}}
    body, secs = _typed_run(schema, {"expr": e}, via_tool=True)
    assert body.get("status") == "completed", body
    assert secs < 0.5, f"a 16-level, 1 KB answer took {secs:.2f}s to check"


def test_redaction_hides_a_password_containing_an_at_sign():
    """F6: the redaction stopped at the first '@' of the password."""
    from webcortex.cli import redacted_manifest
    from webcortex.diagnostics import _without_userinfo
    url = "https://svc:p@ssw0rd@api.internal.example/v1"
    app = WebCortex("redact")
    app.upstream("svc", base_url=url)
    shown = redacted_manifest(app.manifest())["upstreams"]["svc"]["base_url"]
    assert "ssw0rd" not in shown and "ssw0rd" not in _without_userinfo(url)


def test_an_invalid_application_in_an_eval_is_wc101_not_wc001(tmp_path):
    """F7: eval_call raised the build error without 'invalid application:'."""
    app = WebCortex("eval-invalid")
    app.agent("bot", tools=["nope"])
    app.eval("e1", target="bot", input="hi")
    assert app.run_evals(directory=str(tmp_path))["results"][0]["code"] == "WC101"


DEFECT_APP = """
from webcortex import WebCortex
app = WebCortex("defects", port=PORT)
app.anonymous_scopes("*")

@app.get("/echo/{v}")
def echo(v: str):
    return {"v": v}

@app.post("/typed")
def typed(x: float, xs: list[float]):
    return {"x": x, "x_type": type(x).__name__, "xs": xs}

if __name__ == "__main__":
    app.run()
"""


@pytest.fixture(scope="module")
def defect_server(tmp_path_factory):
    with serve(tmp_path_factory.mktemp("defects"), DEFECT_APP) as (base, log):
        yield base


@pytest.mark.parametrize("raw,once", [("%4%31", "%41"), ("b%2%46c", "b%2Fc"), ("%2%45%2%45", "%2E%2E"),
                                      ("%0%30", "%00"), ("%%32%35", "%25"), ("%ff%41", "%ff%41"), ("%aa", "%aa")])
def test_a_path_parameter_is_decoded_exactly_once(defect_server, raw, once):
    """F2: 2.9.0 decoded twice; an undecodable parameter now arrives as sent."""
    status, body = _get(defect_server + "/echo/" + raw, {})
    assert status == 200
    assert json.loads(body)["v"] == once  # one decode of raw; 2.9.0 decodes once more


@pytest.mark.xfail(strict=True, reason="known limit (F5, docs/error-codes.md): hyper writes HTTP/1 protocol "
                                       "errors itself, before WebCortex's service runs, and has no hook for their body")
def test_a_protocol_error_carries_a_wc_code(defect_server):
    port = int(defect_server.rsplit(":", 1)[1])
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    s.sendall(b"GET /echo/x HTTP/1.1\r\nhost: x\r\nno colon here\r\n\r\n")
    data = b""
    while True:
        chunk = s.recv(65536)
        if not chunk:
            break
        data += chunk
    s.close()
    head, _, body = data.partition(b"\r\n\r\n")
    assert head.startswith(b"HTTP/1.1 400")
    assert b"x-webcortex-error" in head.lower() or b'"code"' in body


# ------------------------------------------------- 2.9.1 regression tests


def _post(url: str, raw: bytes) -> tuple[int, bytes]:
    req = urllib.request.Request(url, data=raw, method="POST", headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def test_an_int_past_the_float_range_reaches_a_float_parameter_unchanged(defect_server):
    """F3 over HTTP: 2.9.0 answered 500 WC002 before the handler ran."""
    big = "1" + "0" * 400
    status, body = _post(defect_server + "/typed", f'{{"x": {big}, "xs": [1.5, {big}]}}'.encode())
    assert status == 200, body
    out = json.loads(body)
    assert out["x"] == 10**400 and out["x_type"] == "int" and out["xs"] == [1.5, 10**400]
    # An int a float can hold exactly is still converted; one it cannot is not (F9).
    status, body = _post(defect_server + "/typed", b'{"x": 3, "xs": [9007199254740993]}')
    assert status == 200 and json.loads(body) == {"x": 3.0, "x_type": "float", "xs": [9007199254740993]}


def test_coerce_never_loses_information_and_never_raises():
    """F3 and F9 by example."""
    assert coerce(2**53, float) == 2.0**53 and isinstance(coerce(2**53, float), float)
    assert coerce(2**53 + 1, float) == 2**53 + 1 and isinstance(coerce(2**53 + 1, float), int)
    assert coerce(-(10**400), float) == -(10**400)
    for annotation in (str, decimal.Decimal, float, int, Optional[float], list[str]):
        coerce(10**5000, annotation)
        coerce([10**5000], annotation)


def test_a_typed_answer_with_an_integer_beyond_64_bits_is_refused_as_wc318():
    """F4: the refusal names the range, so the model can send a string instead."""
    body, _ = _typed_run({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]},
                         {"n": 2**64 + 1})
    assert body["status"] == "failed" and body["code"] == "WC318", body
    assert "outside the integers the runtime carries exactly" in json.dumps(body)
    body, _ = _typed_run({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]},
                         {"n": 2**64 - 1})
    assert body["status"] == "completed" and body["data"] == {"n": 2**64 - 1}, body


def test_a_wrong_leaf_deep_in_a_recursive_union_is_named_where_it_is():
    """F1: alternatives are tried fail-fast, and the union member the answer
    meant is the one whose problem the model is told."""
    defs = {
        "Lit": {"type": "object", "properties": {"kind": {"const": "lit"}, "value": {"type": "integer"}},
                "required": ["kind", "value"], "additionalProperties": False},
        "Add": {"type": "object", "properties": {"kind": {"const": "add"}, "left": {"$ref": "#/$defs/E"},
                                                  "right": {"$ref": "#/$defs/E"}},
                "required": ["kind", "left", "right"], "additionalProperties": False},
        "E": {"anyOf": [{"$ref": "#/$defs/Add"}, {"$ref": "#/$defs/Lit"}]},
    }
    schema = {"type": "object", "properties": {"expr": {"$ref": "#/$defs/E"}}, "required": ["expr"], "$defs": defs}
    e = {"kind": "lit", "value": "seven"}
    for _ in range(30):
        e = {"kind": "add", "left": e, "right": {"kind": "lit", "value": 1}}
    body, secs = _typed_run(schema, {"expr": e}, via_tool=True)
    assert body["status"] == "failed" and body["code"] == "WC318", body
    assert "$.expr" + ".left" * 30 + ".value: expected integer, got string" in json.dumps(body)
    assert secs < 5


def test_boot_refuses_output_schemas_the_checker_could_not_follow():
    """F8: 2.9.0 booted these and then refused every answer."""
    selfptr = {"type": "object", "properties": {"x": {"$ref": "#/$defs/D0"}}, "required": ["x"],
               "$defs": {"D0": {"allOf": [{"$ref": "#/$defs/D0/allOf/0"}]}}}
    chain = {"type": "object", "properties": {"x": {"$ref": "#/$defs/D0"}}, "required": ["x"],
             "$defs": {**{f"D{i}": {"$ref": f"#/$defs/D{i + 1}"} for i in range(39)}, "D39": {"type": "integer"}}}
    for schema, why in ((selfptr, "refers back to itself"), (chain, "more than 32 $refs in a row")):
        app = WebCortex("gaps")
        app.agent("a", output=schema)
        with pytest.raises(ValueError, match="invalid application") as e:
            app.check()
        assert why in str(e.value)
