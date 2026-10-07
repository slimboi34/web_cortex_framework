"""Talk to a running WebCortex hub from anywhere: a device driver on a Raspberry
Pi, a backend that wants the insights, a service that drives an agent.

    from webcortex.client import DeviceConnection, subscribe, AgentSocket

    # A camera driver: push frames (and telemetry) over one WebSocket.
    with DeviceConnection("http://hub:8000", "dock", key=KEY) as cam:
        while True:
            ok, frame = capture.read()
            cam.send_frame(Image.from_array(frame, bgr=True))
            cam.send_telemetry({"temp_c": read_temp()})

    # Another system: everything the dock camera and its watchers produce.
    for event in subscribe("http://hub:8000", "dock", key=KEY, frames="meta"):
        if event["type"] == "insight":
            forward(event["output"])

    # An agent, step by step.
    with AgentSocket("http://hub:8000", "/agents/operator", key=KEY) as agent:
        result = agent.ask("look at the dock", on_step=print)

Standard library only: a deliberately small RFC 6455 client, so a device with
nothing but CPython can connect.
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import socket
import ssl
import struct
import urllib.parse
from collections.abc import Callable, Iterator
from typing import Any

from .media import Image

__all__ = ["WebSocket", "DeviceConnection", "AgentSocket", "subscribe", "WebSocketError"]

_GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
OP_CONT, OP_TEXT, OP_BINARY, OP_CLOSE, OP_PING, OP_PONG = 0x0, 0x1, 0x2, 0x8, 0x9, 0xA


class WebSocketError(ConnectionError):
    pass


class WebSocket:
    """A minimal blocking WebSocket client: text, binary, ping/pong, close."""

    #: Largest message accepted from the server, in bytes. A frame header can
    #: announce up to 2**63 bytes; without a ceiling a hostile or broken server
    #: could make the client buffer without bound.
    MAX_MESSAGE = 64 * 1024 * 1024

    def __init__(self, url: str, *, headers: dict[str, str] | None = None, timeout: float = 30.0,
                 max_message: int | None = None) -> None:
        self.max_message = max_message or self.MAX_MESSAGE
        for k, v in (headers or {}).items():
            # A CR or LF in a header would let a value write extra headers.
            if any(c in str(k) + str(v) for c in "\r\n\0"):
                raise ValueError(f"header {k!r} contains a line break or NUL")
        parts = urllib.parse.urlsplit(url)
        if parts.scheme not in ("ws", "wss", "http", "https"):
            raise ValueError(f"not a WebSocket URL: {url!r}")
        secure = parts.scheme in ("wss", "https")
        host = parts.hostname or "localhost"
        port = parts.port or (443 if secure else 80)
        path = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")

        sock = socket.create_connection((host, port), timeout=timeout)
        if secure:
            sock = ssl.create_default_context().wrap_socket(sock, server_hostname=host)
        self._sock = sock
        self._buf = b""

        key = base64.b64encode(os.urandom(16)).decode()
        lines = [
            f"GET {path} HTTP/1.1",
            f"Host: {host}:{port}",
            "Upgrade: websocket",
            "Connection: Upgrade",
            f"Sec-WebSocket-Key: {key}",
            "Sec-WebSocket-Version: 13",
            *(f"{k}: {v}" for k, v in (headers or {}).items()),
        ]
        sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())

        head = self._read_until(b"\r\n\r\n").decode("latin-1")
        status_line, *header_lines = head.split("\r\n")
        status = int(status_line.split()[1]) if len(status_line.split()) > 1 else 0
        if status != 101:
            body = self._buf.decode(errors="replace")
            self._sock.close()
            raise WebSocketError(f"{url}: upgrade refused with {status}: {body[:300]}")
        received = {k.strip().lower(): v.strip() for k, _, v in (h.partition(":") for h in header_lines)}
        # SHA-1 is what RFC 6455 specifies for the handshake; it is not a security control.
        expected = base64.b64encode(hashlib.sha1(key.encode() + _GUID, usedforsecurity=False).digest()).decode()
        if received.get("sec-websocket-accept") != expected:
            self._sock.close()
            raise WebSocketError("bad Sec-WebSocket-Accept from server")

    # -- raw I/O ----------------------------------------------------------

    def _read_until(self, marker: bytes) -> bytes:
        while marker not in self._buf:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise WebSocketError("connection closed during handshake")
            self._buf += chunk
        head, _, self._buf = self._buf.partition(marker)
        return head

    def _read_exact(self, n: int) -> bytes:
        while len(self._buf) < n:
            chunk = self._sock.recv(max(65536, n - len(self._buf)))
            if not chunk:
                raise WebSocketError("connection closed")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        header = bytearray([0x80 | opcode])
        n = len(payload)
        if n < 126:
            header.append(0x80 | n)
        elif n < 1 << 16:
            header.append(0x80 | 126)
            header += struct.pack(">H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", n)
        mask = os.urandom(4)
        header += mask
        # Clients must mask. XOR through an int keeps this fast without numpy.
        masked = (int.from_bytes(payload, "big") ^ int.from_bytes((mask * (n // 4 + 1))[:n], "big")).to_bytes(n, "big") if n else b""
        self._sock.sendall(bytes(header) + masked)

    # -- public -----------------------------------------------------------

    def send_text(self, text: str) -> None:
        self._send_frame(OP_TEXT, text.encode())

    def send_json(self, value: Any) -> None:
        self.send_text(json.dumps(value))

    def send_binary(self, data: bytes) -> None:
        self._send_frame(OP_BINARY, bytes(data))

    def recv(self) -> tuple[int, bytes]:
        """The next data message as `(opcode, payload)`. Answers pings; raises
        `WebSocketError` when the server closes."""
        message, opcode = b"", None
        while True:
            b1, b2 = self._read_exact(2)
            fin, op, n = b1 & 0x80, b1 & 0x0F, b2 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._read_exact(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._read_exact(8))[0]
            if len(message) + n > self.max_message:
                self._sock.close()
                raise WebSocketError(f"message larger than max_message ({self.max_message} bytes)")
            payload = self._read_exact(n)
            if op == OP_PING:
                self._send_frame(OP_PONG, payload)
                continue
            if op == OP_PONG:
                continue
            if op == OP_CLOSE:
                code = struct.unpack(">H", payload[:2])[0] if len(payload) >= 2 else 1005
                raise WebSocketError(f"closed by server ({code}) {payload[2:].decode(errors='replace')}")
            if op != OP_CONT:
                opcode = op
            message += payload
            if fin:
                return opcode or OP_BINARY, message

    def recv_json(self) -> Any:
        while True:
            op, payload = self.recv()
            if op == OP_TEXT:
                return json.loads(payload)

    def settimeout(self, seconds: float | None) -> None:
        self._sock.settimeout(seconds)

    def close(self) -> None:
        try:
            self._send_frame(OP_CLOSE, struct.pack(">H", 1000))
        except OSError:
            pass
        self._sock.close()

    def __enter__(self) -> "WebSocket":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()


def _ws_url(base: str, path: str, key: str | None, **query: str) -> tuple[str, dict]:
    parts = urllib.parse.urlsplit(base.rstrip("/"))
    scheme = "wss" if parts.scheme in ("https", "wss") else "ws"
    q = {k: v for k, v in query.items() if v is not None}
    url = f"{scheme}://{parts.netloc}{parts.path}{path}" + (f"?{urllib.parse.urlencode(q)}" if q else "")
    return url, ({"x-api-key": key} if key else {})


class DeviceConnection:
    """A device's side of the hub: frames and telemetry in, over one socket."""

    def __init__(self, base_url: str, device: str, *, key: str | None = None,
                 prefix: str = "/devices", timeout: float = 30.0) -> None:
        url, headers = _ws_url(base_url, f"{prefix}/{device}/ws", key)
        self.ws = WebSocket(url, headers=headers, timeout=timeout)
        self.ready = self.ws.recv_json()

    def send_frame(self, frame: bytes | Image) -> None:
        """One encoded image. Frames faster than the device's `max_fps` are
        dropped by the hub, silently."""
        data = frame.data if isinstance(frame, Image) else frame
        if data is None:
            raise ValueError("a URL image cannot be sent as a frame")
        self.ws.send_binary(data)

    def send_telemetry(self, reading: Any) -> None:
        self.ws.send_json(reading)

    def close(self) -> None:
        self.ws.close()

    def __enter__(self) -> "DeviceConnection":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()


def subscribe(base_url: str, device: str, *, key: str | None = None, frames: str = "binary",
              prefix: str = "/devices", timeout: float | None = None) -> Iterator[dict]:
    """Every event a device produces: `hello`, `frame` (with the image bytes
    in `event["data"]` unless `frames="meta"`), `telemetry`, `insight`,
    `lagged`. Runs until the connection closes."""
    url, headers = _ws_url(base_url, f"{prefix}/{device}/stream", key,
                           frames="meta" if frames == "meta" else None)
    with WebSocket(url, headers=headers, timeout=timeout or 30.0) as ws:
        ws.settimeout(timeout)
        pending: dict | None = None
        while True:
            op, payload = ws.recv()
            if op == OP_BINARY and pending is not None:
                pending["data"] = payload
                yield pending
                pending = None
                continue
            event = json.loads(payload)
            if event.get("type") == "frame" and frames != "meta":
                pending = event
                continue
            yield event


class AgentSocket:
    """An agent over a WebSocket: each turn streams its steps, then its result."""

    def __init__(self, base_url: str, path: str, *, key: str | None = None, timeout: float = 600.0) -> None:
        url, headers = _ws_url(base_url, path, key)
        self.ws = WebSocket(url, headers=headers, timeout=timeout)
        self.ready = self.ws.recv_json()

    def ask(self, input: str, *, images: list[Image | dict] | None = None,
            session_id: str | None = None, reset: bool = False,
            on_step: Callable[[dict], None] | None = None) -> dict:
        """Run one turn. `on_step` is called with each step as it happens.
        Returns the run result; raises `WebSocketError` on an error message."""
        msg: dict[str, Any] = {"input": input}
        if images:
            msg["images"] = [i.to_json()["$image"] if isinstance(i, Image) else i for i in images]
        if session_id:
            msg["session_id"] = session_id
        if reset:
            msg["reset"] = True
        self.ws.send_json(msg)
        while True:
            event = self.ws.recv_json()
            kind = event.get("type")
            if kind == "step":
                if on_step:
                    on_step(event)
            elif kind == "result":
                return event["result"]
            elif kind == "error":
                raise WebSocketError(event.get("message", "agent error"))

    def close(self) -> None:
        self.ws.close()

    def __enter__(self) -> "AgentSocket":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()
