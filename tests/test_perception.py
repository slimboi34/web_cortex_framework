"""What 2.2 ships for agents that see and machines that move: `webcortex.Image`,
images in and out of agent runs and MCP, `actuator=True`, the emergency stop, and
the robotics starter.

The live tests boot the robotics starter itself against the fake provider, so
the starter a user generates is the thing under test.
"""
from __future__ import annotations

import base64
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import zlib
from pathlib import Path

import pytest

from webcortex import Image, WebCortex, starters
from webcortex._bridge import _encode
from webcortex.cli import main
from webcortex.media import MAX_IMAGE_BYTES, encode_png, sniff

PNG_1x1 = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
)


def _png_pixels(data: bytes) -> tuple[int, int, int, bytes]:
    """Decode what `encode_png` writes: (width, height, colour type, raw scanlines)."""
    assert data.startswith(b"\x89PNG\r\n\x1a\n")
    pos, idat, header = 8, b"", None
    while pos < len(data):
        length = int.from_bytes(data[pos:pos + 4], "big")
        kind, body = data[pos + 4:pos + 8], data[pos + 8:pos + 8 + length]
        crc = int.from_bytes(data[pos + 8 + length:pos + 12 + length], "big")
        assert zlib.crc32(kind + body) == crc, kind
        if kind == b"IHDR":
            header = body
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    assert header is not None
    width, height = int.from_bytes(header[:4], "big"), int.from_bytes(header[4:8], "big")
    return width, height, header[9], zlib.decompress(idat)


# ------------------------------------------------------------------- Image


def test_image_sniffs_the_type_and_serialises_to_the_marker():
    img = Image.from_bytes(PNG_1x1)
    assert img.media_type == "image/png" and img.size == len(PNG_1x1)
    wire = img.to_json()
    assert wire["$image"]["media_type"] == "image/png"
    assert base64.b64decode(wire["$image"]["data"]) == PNG_1x1
    assert sniff(b"\xff\xd8\xff\xe0rest") == "image/jpeg"
    assert sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 ") == "image/webp"
    assert sniff(b"GIF89a...") == "image/gif"


def test_image_refuses_what_a_provider_would():
    with pytest.raises(ValueError, match="unsupported"):
        Image.from_bytes(b"BM not a supported format")
    with pytest.raises(ValueError, match="empty"):
        Image.from_bytes(b"")
    with pytest.raises(ValueError, match="at most"):
        Image.from_bytes(b"\x89PNG\r\n\x1a\n" + b"\0" * MAX_IMAGE_BYTES)
    with pytest.raises(ValueError, match="http"):
        Image.from_url("file:///etc/passwd")
    with pytest.raises(ValueError, match="exactly one"):
        Image()


def test_url_images_are_passed_by_reference():
    img = Image.from_url("https://example.com/frame.jpg")
    assert img.to_json() == {"$image": {"url": "https://example.com/frame.jpg"}}
    assert img.size == 0


def test_from_array_encodes_rgb_rows_as_a_valid_png():
    rows = [[(255, 0, 0), (0, 255, 0)], [(0, 0, 255), (255, 255, 255)]]
    img = Image.from_array(rows)
    width, height, colour, raw = _png_pixels(img.data)
    assert (width, height, colour) == (2, 2, 2)
    # Each scanline: filter byte 0, then RGB triples.
    assert raw == b"\x00\xff\x00\x00\x00\xff\x00" + b"\x00\x00\x00\xff\xff\xff\xff"


def test_from_array_swaps_bgr_for_opencv_frames():
    img = Image.from_array([[(10, 20, 30)]], bgr=True)
    assert _png_pixels(img.data)[3] == b"\x00\x1e\x14\x0a"


def test_from_array_takes_greyscale():
    img = Image.from_array([[0, 128, 255]])
    width, height, colour, raw = _png_pixels(img.data)
    assert (width, height, colour, raw) == (3, 1, 0, b"\x00\x00\x80\xff")


def test_from_array_takes_numpy_when_it_is_installed():
    np = pytest.importorskip("numpy")
    frame = np.zeros((4, 6, 3), dtype=np.uint8)
    frame[..., 0] = 200  # blue, in OpenCV's BGR order
    width, height, colour, raw = _png_pixels(Image.from_array(frame, bgr=True).data)
    assert (width, height, colour) == (6, 4, 2)
    assert raw[1:4] == b"\x00\x00\xc8", "blue ends up in the last channel"
    with pytest.raises(ValueError, match="uint8"):
        Image.from_array(frame.astype(np.float32))


def test_encode_png_checks_its_input():
    with pytest.raises(ValueError, match="channels"):
        encode_png(1, 1, 5, b"\0" * 5)
    with pytest.raises(ValueError, match="expected"):
        encode_png(2, 2, 3, b"\0" * 3)


def test_a_handler_returning_images_encodes_them_anywhere_in_the_result():
    status, body, content_type, _ = _encode({"frames": [Image.from_bytes(PNG_1x1)], "n": 1})
    payload = json.loads(body)
    assert status == 200 and content_type == "application/json"
    assert payload["frames"][0]["$image"]["media_type"] == "image/png"
    assert json.loads(_encode(Image.from_bytes(PNG_1x1))[1])["$image"]["data"]


def test_a_route_returning_an_image_advertises_its_shape():
    app = WebCortex("t")

    @app.get("/snap", tool=True)
    def snap() -> Image:
        return Image.from_bytes(PNG_1x1)

    route = next(r for r in app.manifest()["routes"] if r["path"] == "/snap")
    assert "$image" in route["output_schema"]["properties"]


# ------------------------------------------------------------ declarations


def test_actuator_and_start_halted_reach_the_manifest():
    app = WebCortex("t", start_halted=True)

    @app.post("/arm", tool=True, actuator=True, approval="required", scopes=["operate"])
    def arm(degrees: float) -> dict:
        return {}

    m = app.manifest()
    assert m["server"]["start_halted"] is True
    route = next(r for r in m["routes"] if r["path"] == "/arm")
    assert route["actuator"] is True
    assert all(r["actuator"] is False for r in m["routes"] if r["path"] != "/arm")


def test_agents_take_images_and_cap_how_many_stay_in_context():
    app = WebCortex("t")
    app.agent("eye", max_images=2)
    agent = app.manifest()["agents"][0]
    assert agent["policy"]["max_images"] == 2
    route = next(r for r in app.manifest()["routes"] if r["op"]["kind"] == "agent")
    assert route["input_schema"]["properties"]["images"]["type"] == "array"
    with pytest.raises(ValueError, match="max_images"):
        app.agent("blind", max_images=0)


def test_security_report_flags_an_actuator_a_model_can_move_unattended():
    app = WebCortex("t")

    @app.post("/valve", tool=True, actuator=True)
    def valve(open: bool) -> dict:
        return {}

    sec = app.security_report()
    assert sec["actuators"][0]["name"] == "create_valve"
    warnings = " ".join(sec["actuator_warnings"])
    assert "no authentication" in warnings
    assert "requires no scope" in warnings
    assert "without approval='required'" in warnings


def test_the_robotics_starter_is_gated_scoped_and_boots_halted(tmp_path, monkeypatch):
    for relative, contents in starters.files_for("robotics", "cell", "a cell").items():
        path = tmp_path / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)
    monkeypatch.chdir(tmp_path)
    monkeypatch.syspath_prepend(str(tmp_path))
    monkeypatch.setenv("WEBCORTEX_API_KEY", "test-key-abcdefghijklmnop")
    from webcortex.cli import _load_app

    app = _load_app("api.py")
    sec = app.security_report()
    assert sec["start_halted"] is True
    assert {a["name"] for a in sec["actuators"]} == {"move_joint", "set_gripper"}
    assert sec["actuator_warnings"] == []
    frame = sys.modules["api"].camera_snapshot()["frame"]
    width, height, colour, _ = _png_pixels(frame.data)
    assert (width, height, colour) == (96, 72, 2)


# -------------------------------------------------------------------- live

OPERATOR_KEY = "wc_test_operator_key_0123456789abcdef"
VIEWER_KEY = "wc_test_viewer_key_0123456789abcdefgh"


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, base: str) -> None:
        self.base = base

    def raw(self, path, method="GET", payload=None, key=OPERATOR_KEY):
        data = json.dumps(payload).encode() if payload is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"content-type": "application/json"})
        if key:
            req.add_header("x-api-key", key)
        try:
            with urllib.request.urlopen(req, timeout=30) as res:
                return res.status, json.loads(res.read() or b"null")
        except urllib.error.HTTPError as e:
            body = e.read()
            try:
                return e.code, json.loads(body)
            except ValueError:
                return e.code, body.decode(errors="replace")


@pytest.fixture(scope="module")
def cell():
    port = _free_port()
    workdir = Path(tempfile.mkdtemp(prefix="webcortex-cell-"))
    for relative, contents in starters.files_for("robotics", "cell", "a cell").items():
        path = workdir / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)
    log_path = workdir / "server.log"
    env = {**os.environ, "WEBCORTEX_LOG": "warn", "WEBCORTEX_FAKE_PROVIDER": "1",
           "WEBCORTEX_PORT": str(port), "WEBCORTEX_API_KEY": OPERATOR_KEY,
           "WEBCORTEX_VIEWER_KEY": VIEWER_KEY}
    env.pop("ANTHROPIC_API_KEY", None)
    with log_path.open("w") as log:
        proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py"],
                                cwd=workdir, stdout=log, stderr=subprocess.STDOUT, env=env)
    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 45
    while time.time() < deadline:
        if proc.poll() is not None:
            pytest.fail(f"server exited early:\n{log_path.read_text()}")
        try:
            with urllib.request.urlopen(base + "/_webcortex/health", timeout=1):
                break
        except Exception:
            time.sleep(0.1)
    else:
        proc.kill()
        pytest.fail(f"server never became ready:\n{log_path.read_text()}")
    server = Server(base)
    server.port = port
    server.workdir = workdir
    yield server
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def test_the_cell_boots_halted_and_refuses_to_move(cell):
    status, health = cell.raw("/_webcortex/health")
    assert status == 200 and health["halted"] is True
    status, out = cell.raw("/robot/joints/shoulder", "POST", {"degrees": 10})
    assert status == 423, out
    assert "halted" in json.dumps(out)


def test_release_arms_it_limits_hold_and_scopes_hold(cell):
    status, out = cell.raw("/_webcortex/release", "POST", {})
    assert status == 200 and out["was_halted"] is True
    try:
        status, out = cell.raw("/robot/joints/shoulder", "POST", {"degrees": 30})
        assert status == 200 and out["joints"]["shoulder"] == 30
        status, _ = cell.raw("/robot/joints/elbow", "POST", {"degrees": 500})
        assert status == 422
        status, _ = cell.raw("/robot/joints/shoulder", "POST", {"degrees": 10}, key=VIEWER_KEY)
        assert status == 403
        # The viewer cannot touch the stop either: it is a control-plane route.
        status, _ = cell.raw("/_webcortex/halt", "POST", {}, key=VIEWER_KEY)
        assert status == 403
    finally:
        cell.raw("/_webcortex/halt", "POST", {"reason": "test teardown"})


def test_the_cli_halts_and_releases_the_running_app(cell, monkeypatch, capsys):
    monkeypatch.chdir(cell.workdir)
    monkeypatch.setenv("WEBCORTEX_PORT", str(cell.port))
    monkeypatch.setenv("WEBCORTEX_API_KEY", OPERATOR_KEY)
    assert main(["release", "api.py"]) == 0
    assert cell.raw("/_webcortex/halt")[1]["halted"] is False
    assert main(["halt", "api.py", "--reason", "smoke in the cell"]) == 0
    assert "HALTED: smoke in the cell" in capsys.readouterr().out
    status, state = cell.raw("/_webcortex/halt")
    assert state["halted"] is True and state["state"]["reason"] == "smoke in the cell"


def test_an_agent_sees_what_the_camera_tool_returns(cell):
    status, out = cell.raw("/agents/inspector", "POST", {"input": "tool:camera_snapshot {}"})
    assert status == 200, out
    assert out["output"].endswith("[saw 1 image(s)]"), out["output"]
    step = next(s for s in out["steps"] if s["kind"] == "tool_call")
    assert step["result"]["frame"]["$image"]["bytes"] > 0
    assert "data" not in step["result"]["frame"]["$image"], "no base64 in the step record"


def test_an_agent_takes_images_with_its_input(cell):
    image = {"media_type": "image/png", "data": base64.b64encode(PNG_1x1).decode()}
    status, out = cell.raw("/agents/inspector", "POST", {"input": "what is this?", "images": [image]})
    assert status == 200, out
    assert out["output"] == "inspector echoes: what is this? [saw 1 image(s)]"
    status, out = cell.raw("/agents/inspector", "POST",
                           {"input": "x", "images": [{"media_type": "image/bmp", "data": "AAAA"}]})
    assert status == 400 and "images[0]" in json.dumps(out)


def test_an_actuator_an_agent_reaches_while_halted_fails_after_approval(cell):
    status, out = cell.raw("/agents/operator", "POST", {"input": 'tool:move_joint {"joint": "elbow", "degrees": 60}'})
    assert status == 202 and out["status"] == "awaiting_approval", out
    approval = out["pending_approval"]["approval_id"]
    status, out = cell.raw(f"/_webcortex/approvals/{approval}", "POST", {"approve": True})
    assert status == 200, out
    step = next(s for s in out["steps"] if s["kind"] == "tool_call")
    assert "423" in step["error"], "approval does not get around the emergency stop"


def test_mcp_clients_receive_the_frame_as_image_content(cell):
    status, out = cell.raw("/_webcortex/mcp", "POST", {
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "camera_snapshot", "arguments": {}},
    })
    assert status == 200, out
    content = out["result"]["content"]
    assert content[1]["type"] == "image" and content[1]["mimeType"] == "image/png"
    assert base64.b64decode(content[1]["data"]).startswith(b"\x89PNG")
    status, out = cell.raw("/_webcortex/mcp", "POST", {"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    move = next(t for t in out["result"]["tools"] if t["name"] == "move_joint")
    assert move["annotations"]["actuator"] is True
