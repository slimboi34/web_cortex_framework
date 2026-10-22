"""Helpers the user-workflow scenarios run under. Stdlib only.

Each scenario is a snippet executed in a fresh interpreter whose ``PYTHONPATH``
puts one WebCortex build first (the published wheel or the working tree). The
snippet starts real apps with :func:`serve`, talks to them the way a user or a
client would, and leaves its answer in ``result``.
"""
from __future__ import annotations

import base64
import contextlib
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import textwrap
import time
import urllib.error
import urllib.request
from pathlib import Path


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, port: int, proc: subprocess.Popen, workdir: Path, log: Path) -> None:
        self.port, self.proc, self.workdir, self.log = port, proc, workdir, log
        self.base = f"http://127.0.0.1:{port}"

    # -- HTTP ---------------------------------------------------------------

    def request(self, method: str, path: str, body=None, headers=None, timeout: float = 20.0):
        """(status, headers, parsed body). JSON bodies are parsed; others are text."""
        data = None
        hdrs = dict(headers or {})
        if body is not None:
            if isinstance(body, (bytes, bytearray)):
                data = bytes(body)
            else:
                data = json.dumps(body).encode()
                hdrs.setdefault("content-type", "application/json")
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=hdrs)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, {k.lower(): v for k, v in r.headers.items()}, _parse(r.read())
        except urllib.error.HTTPError as e:
            return e.code, {k.lower(): v for k, v in e.headers.items()}, _parse(e.read())

    def get(self, path, **kw):
        return self.request("GET", path, **kw)

    def post(self, path, body=None, **kw):
        return self.request("POST", path, body, **kw)

    def rpc(self, method: str, params=None, rid=1, headers=None):
        payload = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            payload["params"] = params
        return self.post("/_webcortex/mcp", payload, headers=headers)[2]

    def raw(self, data: bytes, timeout: float = 5.0, read_until_close: bool = True) -> bytes:
        """Send bytes on a fresh socket and return what comes back before close or timeout."""
        out = b""
        with socket.create_connection(("127.0.0.1", self.port), timeout=timeout) as s:
            s.sendall(data)
            try:
                while True:
                    chunk = s.recv(65536)
                    if not chunk:
                        break
                    out += chunk
                    if not read_until_close and b"\r\n\r\n" in out:
                        break
            except (socket.timeout, ConnectionResetError):
                pass
        return out

    # -- WebSocket ----------------------------------------------------------

    def ws(self, path: str, headers=None, timeout: float = 10.0) -> "WS":
        return WS(self.port, path, headers or {}, timeout)

    def alive(self) -> bool:
        if self.proc.poll() is not None:
            return False
        try:
            with urllib.request.urlopen(self.base + "/_webcortex/health", timeout=5) as r:
                return r.status == 200
        except Exception:
            return False

    def logtext(self) -> str:
        return self.log.read_text(errors="replace")


def _parse(raw: bytes):
    if not raw:
        return None
    try:
        return json.loads(raw)
    except ValueError:
        return raw.decode("utf-8", "replace")


@contextlib.contextmanager
def serve(source: str, env=None, fake: bool = True, files=None, ready_timeout: float = 60.0):
    """Run ``source`` as api.py under ``webcortex run`` and yield a :class:`Server`."""
    port = free_port()
    workdir = Path(tempfile.mkdtemp(prefix="wc-case-"))
    (workdir / "api.py").write_text(textwrap.dedent(source))
    for name, text in (files or {}).items():
        path = workdir / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(text if isinstance(text, bytes) else text.encode())
    full_env = {k: v for k, v in os.environ.items()
                if k not in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "WEBCORTEX_FAKE_PROVIDER", "WEBCORTEX_PORT")}
    full_env["WEBCORTEX_LOG"] = "warn"
    if fake:
        full_env["WEBCORTEX_FAKE_PROVIDER"] = "1"
    full_env.update(env or {})
    log = workdir / "server.log"
    with log.open("w") as fh:
        proc = subprocess.Popen([sys.executable, "-m", "webcortex.cli", "run", "api.py", "--port", str(port)],
                                cwd=workdir, stdout=fh, stderr=subprocess.STDOUT, env=full_env)
    server = Server(port, proc, workdir, log)
    deadline = time.time() + ready_timeout
    while True:
        if proc.poll() is not None:
            raise RuntimeError("server exited early:\n" + server.logtext()[-2000:])
        try:
            with urllib.request.urlopen(server.base + "/_webcortex/health", timeout=1):
                break
        except urllib.error.HTTPError:
            break
        except Exception:
            if time.time() > deadline:
                proc.kill()
                raise RuntimeError("server never became ready:\n" + server.logtext()[-2000:])
            time.sleep(0.05)
    try:
        yield server
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


class WS:
    """A minimal RFC 6455 client: enough to drive device and agent sockets."""

    def __init__(self, port: int, path: str, headers: dict, timeout: float) -> None:
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        lines = [f"GET {path} HTTP/1.1", f"Host: 127.0.0.1:{port}", "Upgrade: websocket",
                 "Connection: Upgrade", f"Sec-WebSocket-Key: {key}", "Sec-WebSocket-Version: 13"]
        lines += [f"{k}: {v}" for k, v in headers.items()]
        self.sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(1)
            if not chunk:
                break
            head += chunk
        self.status = int(head.split(b" ", 2)[1]) if head.startswith(b"HTTP/") else 0
        self.head = head.decode("latin-1")

    def send(self, payload, opcode=None, fin=True, mask=True, rsv=0):
        if isinstance(payload, str):
            payload, opcode = payload.encode(), opcode or 1
        opcode = 2 if opcode is None else opcode
        first = (0x80 if fin else 0) | (rsv << 4) | opcode
        n = len(payload)
        if n < 126:
            header = struct.pack("!BB", first, (0x80 if mask else 0) | n)
        elif n < 65536:
            header = struct.pack("!BBH", first, (0x80 if mask else 0) | 126, n)
        else:
            header = struct.pack("!BBQ", first, (0x80 if mask else 0) | 127, n)
        if mask:
            key = os.urandom(4)
            payload = bytes(b ^ key[i % 4] for i, b in enumerate(payload)) if n < 1 << 16 else _mask_fast(payload, key)
            header += key
        self.sock.sendall(header + payload)

    def recv(self, timeout: float = 10.0):
        """(opcode, payload) of the next frame, or (None, b'') on close/timeout."""
        self.sock.settimeout(timeout)
        try:
            b1, b2 = self._exact(2)
            n = b2 & 0x7F
            if n == 126:
                n = struct.unpack("!H", self._exact(2))[0]
            elif n == 127:
                n = struct.unpack("!Q", self._exact(8))[0]
            key = self._exact(4) if b2 & 0x80 else None
            data = self._exact(n)
            if key:
                data = bytes(b ^ key[i % 4] for i, b in enumerate(data))
            return b1 & 0x0F, data
        except (socket.timeout, ConnectionError, OSError, EOFError):
            return None, b""

    def _exact(self, n: int) -> bytes:
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise EOFError
            buf += chunk
        return buf

    def close(self):
        with contextlib.suppress(OSError):
            self.sock.close()


def _mask_fast(payload: bytes, key: bytes) -> bytes:
    n = len(payload)
    k = (key * (n // 4 + 1))[:n]
    return (int.from_bytes(payload, "big") ^ int.from_bytes(k, "big")).to_bytes(n, "big")


def cli(*args: str, cwd=None, env=None, timeout: float = 60.0):
    """Run ``webcortex <args>``: (returncode, stdout, stderr)."""
    full_env = {**os.environ, **(env or {})}
    p = subprocess.run([sys.executable, "-m", "webcortex.cli", *args], cwd=cwd, env=full_env,
                       capture_output=True, text=True, timeout=timeout)
    return p.returncode, p.stdout, p.stderr


def throughput(port: int, path: str, seconds: float = 3.0, threads: int = 8) -> float:
    """Requests per second over keep-alive connections from ``threads`` clients."""
    import http.client
    import threading

    counts = [0] * threads
    stop = time.perf_counter() + seconds

    def worker(i: int) -> None:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        while time.perf_counter() < stop:
            conn.request("GET", path)
            r = conn.getresponse()
            r.read()
            if r.status != 200:
                raise RuntimeError(f"{path} answered {r.status}")
            if r.getheader("connection", "").lower() == "close":
                conn.close()
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            counts[i] += 1
        conn.close()

    started = time.perf_counter()
    pool = [threading.Thread(target=worker, args=(i,)) for i in range(threads)]
    for t in pool:
        t.start()
    for t in pool:
        t.join()
    return sum(counts) / (time.perf_counter() - started)
