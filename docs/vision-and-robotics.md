# Vision and robotics

An agent that can call a camera should see the picture rather than a
description of it. A tool that moves a motor needs more protection than a tool
that writes a row. WebCortex 2.2 adds both.

WebCortex still **does not run models**. A frame goes to whichever model the
agent uses (Claude, or a local vision model behind an OpenAI-compatible
server). Detectors, trackers and SLAM stay in your own code, as tools.

```bash
webcortex new cell -t robotics
cd cell
export WEBCORTEX_API_KEY=$(webcortex keygen)
webcortex dev            # boots HALTED
webcortex release        # in another terminal: arm the actuators
```

## Images out of tools

Return a `webcortex.Image` from a handler, either on its own or nested anywhere
in the result:

```python
from webcortex import Image

@app.get("/camera/snapshot", tool=True, tool_name="camera_snapshot", scopes=["observe"])
def camera_snapshot() -> dict:
    ok, frame = cap.read()                                 # OpenCV: BGR uint8
    return {"frame": Image.from_array(frame, bgr=True), "taken_at": time.time()}
```

| Constructor | Use it for |
|---|---|
| `Image.from_array(pixels, bgr=False)` | NumPy `uint8` arrays `(h, w)`, `(h, w, 3)`, `(h, w, 4)`, or nested lists. Encoded as PNG with the standard library, so neither NumPy nor Pillow is a dependency |
| `Image.from_bytes(data, media_type=None)` | Bytes that are already encoded. The type is sniffed. For example `cv2.imencode(".jpg", frame)[1].tobytes()`, which is much smaller than PNG for photos |
| `Image.from_path(path)` | A file on disk |
| `Image.from_pil(img, format="PNG")` | A Pillow image |
| `Image.from_url(url)` | An image the model's provider fetches itself |

What each caller receives:

- **An agent in the same app** gets the picture inside the tool result, in
  both wire formats. Anthropic models get an image block inside
  `tool_result`. OpenAI-compatible servers (Ollama, vLLM, LM Studio) get the
  tool message followed by a user message with `image_url` parts.
- **An MCP client** such as Claude Code, Claude Desktop or Cursor gets an
  `image` content item after the text. Plug the app in with
  `webcortex mcp-config` and your coding agent can look through the camera.
- **A plain HTTP caller** gets JSON: `{"$image": {"media_type": "image/png", "data": "<base64>"}}`.

The text the model reads keeps a placeholder (`[image 1: image/png, 2604 bytes — attached]`)
where the image was, so the rest of the result still makes sense.
`tool_result_limit` applies to the text only, so a frame is never cut in half.
Step records and the audit log store each image's size, never its base64.

## Images into agents

Every agent endpoint accepts up to 16 images with the input:

```bash
curl -X POST localhost:8000/agents/inspector -H "x-api-key: $WEBCORTEX_API_KEY" \
  -H 'content-type: application/json' -d '{
    "input": "Is this weld acceptable?",
    "images": [{"media_type": "image/jpeg", "data": "'"$(base64 -i weld.jpg)"'"}]
  }'
```

Each image is either `{"media_type", "data"}` or `{"url"}`. A malformed one is
rejected with a 400 that names the bad item (`images[2]: …`). Each image can be
at most 5 MB.

## Frames are expensive context

Every step of a run sends the whole conversation again. If an agent watches a
process by calling the camera ten times, that means ten frames on the tenth
step. `max_images` (default 4) is how many images stay in context as pixels.
Older ones become a line of text such as `[earlier image (image/png, 2604 bytes) dropped from context to save tokens]`,
and `usage.images_dropped` counts them.

```python
app.agent("watcher", tools=["camera_snapshot"], max_images=2)
```

## Actuators and the emergency stop

```python
@app.post("/robot/joints/{joint}", tool=True, tool_name="move_joint", scopes=["operate"],
          actuator=True, approval="required")
def move_joint(joint: str, degrees: float) -> dict:
    low, high = LIMITS[joint]
    if not low <= degrees <= high:
        raise HTTPError(422, f"{joint} must stay within [{low}, {high}]")
    return driver.move(joint, degrees)
```

`actuator=True` marks a route that acts on the physical world. While the
emergency stop is engaged, every actuator answers **423 Locked** before its
handler runs, however the call arrives:

| Path | While halted |
|---|---|
| HTTP | 423 |
| An agent's tool call | the model receives the 423 as a tool error |
| A behaviour's `ctx.call` or a flow step | the call fails with 423 |
| MCP `tools/call` | `isError: true` |
| An approval granted after the halt | the tool call fails with 423 when the run resumes |

The check lives in `App::dispatch`, the one function every path goes through.
Reads such as cameras and sensors keep working, so an operator can still see
what is going on.

```bash
webcortex halt --reason "person in the cell"     # or POST /_webcortex/halt {"reason": "..."}
webcortex halt --status
webcortex release                                # or POST /_webcortex/release
```

Halting and releasing need the `webcortex:admin` scope, like every other
control-plane route, and both are written to the audit log. The halt is held in
memory. So that a crash and restart does not come back with every motor live,
use `WebCortex(..., start_halted=True)` (the robotics starter does). The app
then boots halted and waits for an operator to release it.

!!! warning "The emergency stop is software"
    It stops WebCortex from sending commands. It does not replace a
    hardware e-stop, a safety-rated controller, or your driver's own limits.
    Keep joint and speed limits in code (as `move_joint` does above), and do
    not rely on a prompt for them.

## What `webcortex security` checks

It lists every actuator and warns when:

- actuators exist but no authentication is configured, which means anyone who
  can reach the port can move hardware and release the stop;
- an actuator requires no scope;
- an actuator is an agent tool without `approval="required"`, so a model can
  move it unattended.

The robotics starter produces none of these warnings.

## Connecting real hardware

The starter's `Cell` class is the only simulated part. Replace it with any of:

- **Serial or USB** microcontrollers (`pyserial`), such as an Arduino or ESP32
  running your firmware;
- **ROS 2**, via an `rclpy` node in the same process: publish to a command
  topic in `move_joint`, and keep the latest `Image` message for
  `camera_snapshot`;
- **PLCs** over Modbus/TCP (`pymodbus`) or OPC UA;
- **Vendor SDKs** for arms, grippers and cameras.

Handlers run on the free-threaded worker pool, so a driver must be
thread-safe. Guard it with a lock, as `Cell` does.

## Use cases

- **Visual inspection:** an inspector agent photographs each part, records a
  verdict in a Rust-served resource, and hands borderline cases to a person.
- **Lab automation:** an agent runs a protocol on a liquid handler, and every
  dispense waits for approval until the protocol has been validated.
- **Remote operation:** an operator works through Claude Code over MCP. The
  model sees the camera, proposes moves, and each move waits for a click.
- **Facilities and IoT:** valves, relays and HVAC set-points as actuators,
  sensors as context providers, and one emergency stop for all of them.
- **Document and screen understanding:** screenshots, scans or charts sent as
  `images`, with no hardware involved.
