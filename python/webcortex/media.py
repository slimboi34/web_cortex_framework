"""Images a tool can return, and an agent can see.

WebCortex runs no vision model. It carries pixels to the model that does: a
handler returns an `Image` — on its own or anywhere inside its result — and an
agent calling that tool receives the picture beside the rest of the result,
over either provider wire format and over MCP.

    from webcortex import Image

    @app.get("/camera/snapshot", tool=True)
    def snapshot() -> dict:
        frame = camera.read()                      # a NumPy array from OpenCV
        return {"frame": Image.from_array(frame, bgr=True), "exposure_ms": 8}

Over plain HTTP the same result is JSON: the image is
`{"$image": {"media_type": "image/png", "data": "<base64>"}}`, which is also
how an agent request carries images (`{"input": "...", "images": [...]}`).

Stdlib only. NumPy arrays and PIL images are accepted by duck typing, so
neither is a dependency.
"""

from __future__ import annotations

import base64
import mimetypes
import struct
import zlib
from pathlib import Path
from typing import Any

__all__ = ["Image", "MAX_IMAGE_BYTES", "MEDIA_TYPES"]

#: Media types every supported provider accepts.
MEDIA_TYPES = ("image/png", "image/jpeg", "image/gif", "image/webp")

#: Providers reject larger images; the runtime refuses them with a clearer error.
MAX_IMAGE_BYTES = 5 * 1024 * 1024

_MARKER = "$image"


def sniff(data: bytes) -> str | None:
    """The media type of encoded image bytes, from their magic number."""
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if data.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if data.startswith((b"GIF87a", b"GIF89a")):
        return "image/gif"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    return None


class Image:
    """An image for a model to look at: encoded bytes plus a media type, or a URL."""

    __slots__ = ("data", "media_type", "url")

    def __init__(
        self,
        data: bytes | None = None,
        media_type: str | None = None,
        *,
        url: str | None = None,
    ) -> None:
        if (data is None) == (url is None):
            raise ValueError("Image needs exactly one of `data` or `url`")
        if url is not None:
            if not url.startswith(("https://", "http://")):
                raise ValueError(f"Image url must be http(s): {url!r}")
            self.data, self.media_type, self.url = None, media_type, url
            return
        data = bytes(data)  # type: ignore[arg-type]
        if not data:
            raise ValueError("Image data is empty")
        if len(data) > MAX_IMAGE_BYTES:
            raise ValueError(
                f"Image is {len(data)} bytes; providers accept at most {MAX_IMAGE_BYTES}. "
                "Downscale it, or encode it as JPEG."
            )
        media_type = media_type or sniff(data)
        if media_type not in MEDIA_TYPES:
            raise ValueError(
                f"unsupported image type {media_type!r}; encode as one of {', '.join(MEDIA_TYPES)}"
            )
        self.data, self.media_type, self.url = data, media_type, None

    # -- constructors ---------------------------------------------------

    @classmethod
    def from_bytes(cls, data: bytes, media_type: str | None = None) -> "Image":
        """Already-encoded PNG, JPEG, GIF or WebP bytes. The type is sniffed if omitted."""
        return cls(data, media_type)

    @classmethod
    def from_path(cls, path: str | Path) -> "Image":
        """An image file on disk."""
        path = Path(path)
        data = path.read_bytes()
        return cls(data, sniff(data) or mimetypes.guess_type(path.name)[0])

    @classmethod
    def from_url(cls, url: str) -> "Image":
        """An image the model's provider fetches itself. Nothing is downloaded here."""
        return cls(url=url)

    @classmethod
    def from_pil(cls, image: Any, format: str = "PNG", **save_kwargs: Any) -> "Image":
        """A PIL/Pillow image, encoded as PNG (or `format="JPEG"` for photos)."""
        import io

        buf = io.BytesIO()
        image.save(buf, format=format, **save_kwargs)
        return cls(buf.getvalue(), f"image/{format.lower().replace('jpg', 'jpeg')}")

    @classmethod
    def from_array(cls, pixels: Any, *, bgr: bool = False) -> "Image":
        """Raw pixels, encoded as PNG.

        `pixels` is a NumPy `uint8` array shaped `(h, w)`, `(h, w, 3)` or
        `(h, w, 4)` — a camera frame — or the same as nested lists. Pass
        `bgr=True` for OpenCV frames, whose channels are blue-green-red.
        """
        width, height, channels, raw = _raw_pixels(pixels, bgr)
        return cls(encode_png(width, height, channels, raw), "image/png")

    # -- wire form ------------------------------------------------------

    def __webcortex_json__(self) -> dict:
        """The marker the runtime lifts out of a tool result."""
        if self.url is not None:
            return {_MARKER: {"url": self.url}}
        return {
            _MARKER: {
                "media_type": self.media_type,
                "data": base64.b64encode(self.data or b"").decode("ascii"),
            }
        }

    to_json = __webcortex_json__

    @classmethod
    def model_json_schema(cls) -> dict:
        """What a route returning an `Image` advertises in OpenAPI and tool schemas."""
        return {
            "type": "object",
            "title": "Image",
            "properties": {
                _MARKER: {
                    "type": "object",
                    "properties": {
                        "media_type": {"type": "string", "enum": list(MEDIA_TYPES)},
                        "data": {"type": "string", "contentEncoding": "base64"},
                        "url": {"type": "string", "format": "uri"},
                    },
                }
            },
            "required": [_MARKER],
        }

    @property
    def size(self) -> int:
        """Encoded size in bytes; 0 for a URL image."""
        return len(self.data or b"")

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Image) and (self.data, self.media_type, self.url) == (
            other.data, other.media_type, other.url,
        )

    def __repr__(self) -> str:
        if self.url is not None:
            return f"Image(url={self.url!r})"
        return f"Image({self.media_type}, {self.size} bytes)"


# ---------------------------------------------------------------------------
# PNG encoding, stdlib only
# ---------------------------------------------------------------------------

_COLOUR_TYPE = {1: 0, 2: 4, 3: 2, 4: 6}  # channels -> PNG colour type


def _raw_pixels(pixels: Any, bgr: bool) -> tuple[int, int, int, bytes]:
    shape = getattr(pixels, "shape", None)
    if shape is not None:  # NumPy, or anything shaped like it
        if str(getattr(pixels, "dtype", "uint8")) != "uint8":
            raise ValueError(
                f"Image.from_array needs uint8 pixels, got {pixels.dtype}; "
                "scale and convert with `(a * 255).astype('uint8')` first"
            )
        if len(shape) not in (2, 3):
            raise ValueError(f"Image.from_array needs a (h, w) or (h, w, c) array, got shape {shape}")
        height, width = int(shape[0]), int(shape[1])
        channels = int(shape[2]) if len(shape) == 3 else 1
        if bgr and channels in (3, 4):
            order = [2, 1, 0] if channels == 3 else [2, 1, 0, 3]
            pixels = pixels[..., order]
        return width, height, channels, pixels.tobytes()

    rows = [list(r) for r in pixels]
    if not rows or not rows[0]:
        raise ValueError("Image.from_array got no pixels")
    height, width = len(rows), len(rows[0])
    first = rows[0][0]
    channels = 1 if isinstance(first, int) else len(first)
    out = bytearray()
    for row in rows:
        if len(row) != width:
            raise ValueError("Image.from_array rows must all be the same width")
        for px in row:
            values = [px] if channels == 1 else list(px)
            if bgr and channels in (3, 4):
                values[0], values[2] = values[2], values[0]
            out.extend(values)
    return width, height, channels, bytes(out)


def encode_png(width: int, height: int, channels: int, raw: bytes) -> bytes:
    """Encode 8-bit pixels as PNG. `raw` is row-major, channels interleaved."""
    if channels not in _COLOUR_TYPE:
        raise ValueError(f"PNG needs 1, 2, 3 or 4 channels, got {channels}")
    stride = width * channels
    if len(raw) != stride * height:
        raise ValueError(f"expected {stride * height} bytes of pixels, got {len(raw)}")
    # Filter type 0 (none) on every scanline.
    scanlines = b"".join(b"\x00" + raw[y * stride:(y + 1) * stride] for y in range(height))

    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    header = struct.pack(">IIBBBBB", width, height, 8, _COLOUR_TYPE[channels], 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(scanlines, 6))
        + chunk(b"IEND", b"")
    )
