"""A misdeclared app: an agent names a tool that does not exist (a boot error)."""
from webcortex import WebCortex

app = WebCortex("broken")
app.static("GET", "/ping", {"pong": True}, tool=True, tool_name="ping")
app.agent("helper", tools=["pnig"])
