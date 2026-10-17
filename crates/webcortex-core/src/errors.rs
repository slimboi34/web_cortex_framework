//! The error catalogue: one stable code per failure a user can meet.
//!
//! Every error that leaves the runtime — an HTTP JSON body, an MCP tool error,
//! an agent run that ends badly, a CLI failure — carries a `code` from this
//! table and a `hint` saying what to do about it. Codes are **stable**: once
//! released, a code keeps its meaning, so a client, a test or a coding agent
//! can branch on `WC402` rather than on the wording of a message.
//!
//! This table is the single source of truth. Python reads it through
//! `_core.error_catalogue()` (`webcortex explain`, `webcortex.errors`), and a
//! test on each side scans the sources for `WC` codes and fails on any that is
//! not listed here.
//!
//! Ranges:
//!
//! | range | area |
//! |---|---|
//! | `WC0xx` | internal faults |
//! | `WC1xx` | configuration, boot and the environment |
//! | `WC2xx` | routing, HTTP and request input |
//! | `WC3xx` | agents, behaviours, flows and model providers |
//! | `WC4xx` | budgets and limits |
//! | `WC5xx` | devices, actuators and the emergency stop |
//! | `WC6xx` | authentication and security |

use serde_json::{Value, json};

/// One catalogue entry.
#[derive(Debug, Clone, Copy)]
pub struct ErrorInfo {
    pub code: &'static str,
    /// A short name, stable enough to show as a heading.
    pub title: &'static str,
    /// `internal | config | http | agent | limits | devices | auth`
    pub category: &'static str,
    /// The HTTP status this failure is answered with, when it is an HTTP failure.
    pub status: Option<u16>,
    /// What to do about it. Written for the person (or model) reading the error.
    pub hint: &'static str,
}

macro_rules! e {
    ($code:literal, $cat:literal, $status:expr, $title:literal, $hint:literal) => {
        ErrorInfo { code: $code, title: $title, category: $cat, status: $status, hint: $hint }
    };
}

/// The catalogue, ordered by code.
pub static CATALOGUE: &[ErrorInfo] = &[
    // ---- WC0xx internal -------------------------------------------------
    e!("WC001", "internal", Some(500), "Internal error",
       "Something failed inside the server. The server log has the full error under this response's x-request-id; set WEBCORTEX_DEBUG_ERRORS=1 locally to return Python tracebacks."),
    e!("WC002", "internal", Some(500), "Python handler raised",
       "An unhandled exception escaped a Python handler; its traceback is in the server log. Raise webcortex.HTTPError(status, message) for failures the caller should see."),
    e!("WC003", "internal", Some(500), "Behaviour failed",
       "A behaviour raised an exception; its traceback is in the server log. Use ctx.halt(reason) to stop a behaviour on purpose."),
    e!("WC004", "internal", Some(500), "Template failed to render",
       "A page template is missing or raised while rendering; check the template name and the data the page binds."),

    // ---- WC1xx configuration --------------------------------------------
    e!("WC101", "config", None, "Invalid application",
       "The app's declarations do not validate. The message names the declaration; fix it in api.py and run `webcortex check`."),
    e!("WC102", "config", None, "App failed to load",
       "Importing the app module raised, or it does not define `app`. Run `python api.py` to see the traceback, or pass module:attr."),
    e!("WC103", "config", None, "Core extension mismatch",
       "The compiled extension (webcortex._core) is missing or from a different version than the Python package. Reinstall the wheel, or run `maturin develop` in a working copy."),
    e!("WC104", "config", None, "Port already in use",
       "Another process is listening on the app's port. Stop it, or choose another with --port or WEBCORTEX_PORT."),
    e!("WC105", "config", Some(500), "No database configured",
       "A route needs SQL but WebCortex(...) was given no database=. Pass database=\"sqlite://./app.db\" or set WEBCORTEX_DATABASE_URL."),
    e!("WC106", "config", None, "Environment variable not set",
       "A declaration names an environment variable that is unset (an API key, a bearer token, a provider key). Export it or add it to .env; values are never printed."),
    e!("WC107", "config", None, "Path does not exist",
       "A configured directory or database path does not exist. Create it or correct the path."),
    e!("WC108", "config", None, "Running app not reachable",
       "Nothing answered at the app's address. Start it with `webcortex dev`, or point the command at it with --url or WEBCORTEX_HOST/WEBCORTEX_PORT."),

    // ---- WC2xx routing and HTTP -----------------------------------------
    e!("WC200", "http", None, "Request failed",
       "The handler answered with an error status and no more specific code. Read the message; a handler can raise HTTPError(status, message, code=..., hint=...) to say more."),
    e!("WC201", "http", Some(404), "No such route",
       "No route matches this path. `webcortex check` lists every route, and GET /_webcortex/routes lists them on a running app."),
    e!("WC202", "http", Some(405), "Method not allowed",
       "The path exists but not for this method; the `allow` header lists the methods it takes."),
    e!("WC203", "http", Some(504), "Request timed out",
       "The handler ran past request_timeout (or agent_timeout for agents, behaviours and flows). Raise the limit on WebCortex(...) or make the handler faster."),
    e!("WC204", "http", Some(413), "Request body too large",
       "The body is over the 32 MB limit. Send large files to a static or device ingest route in pieces, or reference them by URL."),
    e!("WC205", "http", Some(400), "Bad request",
       "The request could not be used as sent: check the JSON body and parameter types against the route's schema in /_webcortex/openapi.json."),
    e!("WC206", "http", Some(422), "Missing or invalid parameter",
       "A required parameter was absent or could not be read. Send every required field named in the route's input schema (path, query string or JSON body)."),
    e!("WC207", "http", Some(404), "Not found",
       "The route exists but the thing it looks up does not: check the id or key you sent."),
    e!("WC208", "http", Some(400), "Tool arguments address another route",
       "A path argument would have routed this tool call to a different route. Pass a plain value for the path parameter, not a path."),
    e!("WC209", "http", Some(400), "Invalid path parameter",
       "A path parameter contained a slash, '..', or a control character. Path parameters are single segments."),
    e!("WC210", "http", Some(502), "Upstream failed",
       "A proxied upstream or a pulled camera did not answer properly. Check that it is reachable from the server and that its bearer_env is set."),
    e!("WC211", "http", Some(400), "Database rejected the query",
       "SQLite refused the statement or its values (a constraint, a type). The message is SQLite's; check the values you sent."),
    e!("WC212", "http", Some(404), "No such control endpoint",
       "That path is not part of the control plane. GET /_webcortex/health, /status, /routes, /tools, /usage and /audit are the common ones."),
    e!("WC213", "http", Some(426), "WebSocket required",
       "This route is a WebSocket. Connect with `Upgrade: websocket` (webcortex.client.WebSocket or DeviceConnection)."),
    e!("WC214", "http", Some(409), "Conflict",
       "The request conflicts with the current state of the resource; re-read it and retry."),
    e!("WC215", "http", Some(503), "Unavailable",
       "A subsystem this route needs is not running in this process (for example the agent runtime). Check the server log at boot."),
    e!("WC216", "http", Some(408), "Request body too slow",
       "The client announced a body but did not finish sending it within request_timeout (agent_timeout on agent, behaviour and flow routes). Send the whole body promptly, or raise the limit on WebCortex(...)."),

    // ---- WC3xx agents and providers -------------------------------------
    e!("WC300", "agent", Some(502), "Agent run failed",
       "The run stopped on an error. The last step's `error` says which call failed; GET /_webcortex/status shows recent runs."),
    e!("WC301", "agent", Some(502), "Provider key not set",
       "The model's provider needs an API key that is not set. Export ANTHROPIC_API_KEY or OPENAI_API_KEY (or the provider's api_key_env), use a local ollama/<model>, or set WEBCORTEX_FAKE_PROVIDER=1 for tests. `webcortex doctor` checks every model."),
    e!("WC302", "agent", Some(502), "Undeclared provider",
       "The model name has a prefix no provider answers to. Declare it with app.provider(name, base_url=...), or use ollama/, openai/ or anthropic/."),
    e!("WC303", "agent", Some(502), "Provider unreachable",
       "The model provider could not be reached. Check the base URL and network; for Ollama run `ollama serve` (OLLAMA_HOST). `webcortex doctor --online` probes each provider."),
    e!("WC304", "agent", Some(502), "Provider returned an error",
       "The model provider answered with an error status (bad model name, overloaded, invalid request). The message carries the provider's own detail."),
    e!("WC305", "agent", Some(502), "Unreadable provider response",
       "The provider answered with something that is not its wire format. Check that the base URL points at the API, not a web page."),
    e!("WC306", "agent", Some(400), "Malformed agent request",
       "An agent route takes {\"input\": str, \"session_id\"?: str, \"reset\"?: bool, \"images\"?: [...]}; `input` is required and must be a string."),
    e!("WC307", "agent", None, "Tool not available to agent",
       "The model asked for a tool this agent was not given. Add it to the agent's tools=[...] if it should have it."),
    e!("WC308", "agent", None, "Tool call failed",
       "A tool the agent called answered with an error; its code and message are in the step. The model sees the error and may retry."),
    e!("WC309", "agent", None, "Approval required",
       "This tool is gated with approval=\"required\". A human decides with POST /_webcortex/approvals/{id}; it cannot be called directly over MCP."),
    e!("WC310", "agent", Some(404), "Approval not found",
       "No pending approval has that id: it was already decided, or it expired (approval_ttl_secs). GET /_webcortex/approvals lists the pending ones."),
    e!("WC311", "agent", Some(502), "Context provider failed",
       "A context provider named in context=[...] failed while the run started. Check its SQL or Python handler."),
    e!("WC312", "agent", None, "Response cut off at max_tokens",
       "The model ran out of max_tokens mid-answer, and no tool call in that response was run. Raise max_tokens, or reasoning=\"none\" for local thinking models."),
    e!("WC313", "agent", Some(400), "Invalid image input",
       "An image in `images` is not a supported {\"$image\": ...} marker, is too large, or names an internal host. Use webcortex.Image."),
    e!("WC314", "agent", None, "Unknown tool",
       "No exposed tool has this name. tools/list (MCP) or GET /_webcortex/tools shows the names this caller can use."),
    e!("WC315", "agent", Some(500), "Flow step failed",
       "A step of the flow failed or was refused; the message names the step. Run that tool alone to see its error."),
    e!("WC316", "agent", None, "Watcher run failed",
       "A watcher's agent run failed; the insight carries the error. GET /_webcortex/status shows the watcher's recent runs."),

    // ---- WC4xx budgets and limits ---------------------------------------
    e!("WC401", "limits", Some(429), "Rate limited",
       "Too many requests from this caller. Wait for retry-after seconds, or raise app.rate_limit(per_second=..., burst=...)."),
    e!("WC402", "limits", None, "Token budget exhausted",
       "The run hit its token_budget (its own, or the request tree's shared one). Raise token_budget, use a cheaper model alias, or bound context with tool_result_limit / context_window."),
    e!("WC403", "limits", Some(508), "Nesting too deep",
       "Agents, behaviours or flows called each other past max_invocation_depth: usually something calling itself. Break the cycle."),
    e!("WC404", "limits", None, "Step limit reached",
       "The run used max_steps before the model stopped asking for tools. Raise max_steps, or give the agent fewer, sharper tools."),
    e!("WC405", "limits", None, "MCP batch too large",
       "A JSON-RPC batch may hold at most 32 messages. Split it."),

    // ---- WC5xx devices and actuators ------------------------------------
    e!("WC501", "devices", Some(404), "Unknown device",
       "No device has that name. Declare it with app.camera(name) or app.sensor(name); GET /_webcortex/devices lists them."),
    e!("WC502", "devices", Some(423), "Actuators halted",
       "The emergency stop is engaged, so nothing that moves hardware runs. An operator releases it with `webcortex release` or POST /_webcortex/release (admin scope)."),
    e!("WC503", "devices", Some(404), "No frame yet",
       "The device has not sent a frame. Open /devices/<name>/connect on the camera, or push frames to its ingest route."),
    e!("WC504", "devices", Some(429), "Frame faster than max_fps",
       "The device is sending faster than its max_fps; the frame was dropped. Slow the sender or raise max_fps."),
    e!("WC505", "devices", Some(422), "Invalid frame or telemetry",
       "The frame is not a PNG/JPEG/GIF/WebP, is over max_frame_bytes, or the telemetry is not a JSON body (a body that is not JSON at all is answered 400; the rest 422)."),
    e!("WC506", "devices", Some(502), "Camera unreachable",
       "A pulled IP camera did not answer with a snapshot. Check its URL, its bearer_env, and that it is reachable from the server."),

    // ---- WC6xx authentication and security ------------------------------
    e!("WC601", "auth", Some(401), "Authentication required",
       "This route needs a credential. Send the API key in the x-api-key header (or Authorization: Bearer <jwt>)."),
    e!("WC602", "auth", Some(403), "Missing scope",
       "The caller is authenticated but lacks a scope this route requires; the message names it. Use a key whose app.api_key(..., scopes=[...]) includes it."),
    e!("WC603", "auth", Some(401), "Invalid credential",
       "The API key or token was not accepted (unknown, expired or malformed). Check the key, or mint one with `webcortex keygen` and register it with app.api_key(env_var, ...)."),
    e!("WC604", "auth", Some(401), "Admin scope required",
       "The /_webcortex control plane needs the 'webcortex:admin' scope once any auth is configured. Use an admin key: app.api_key(env, id=..., scopes=['webcortex:admin'])."),
    e!("WC605", "auth", Some(403), "Cross-origin request refused",
       "A browser on another origin sent a state-changing request or a WebSocket upgrade. List that origin in app.cors(...) if it should be allowed."),
    e!("WC606", "auth", Some(403), "CORS origin not allowed",
       "The preflight's Origin is not in app.cors(...). Add it there."),
    e!("WC607", "auth", Some(401), "Needs an authenticated caller",
       "Anonymous callers share one identity, so @principal-scoped data and agent sessions need a real credential."),
    e!("WC608", "auth", None, "No authentication configured",
       "Every route is public. Add app.api_key('WEBCORTEX_API_KEY', id='service', scopes=[...]) before exposing the app beyond localhost."),
    e!("WC609", "auth", Some(401), "Unauthorized (raised by the app)",
       "The app's own code answered 401 (for example a wrong password at login); its message says why. WebCortex's own credential checks answer WC601, WC603 or WC604 instead."),
    e!("WC610", "auth", Some(403), "Forbidden (raised by the app)",
       "The app's own code refused this caller with 403; its message says why. A missing WebCortex scope answers WC602 and a cross-origin refusal WC605 instead."),
];

/// The entry for a code.
pub fn lookup(code: &str) -> Option<&'static ErrorInfo> {
    CATALOGUE.iter().find(|e| e.code == code)
}

/// The catalogue hint for a code, or an empty string for an unknown one.
pub fn hint(code: &str) -> &'static str {
    lookup(code).map(|e| e.hint).unwrap_or("")
}

/// The code an error with no more specific one gets, from its status alone.
pub fn code_for_status(status: u16) -> &'static str {
    match status {
        400 => "WC205",
        401 => "WC609",
        403 => "WC610",
        404 => "WC207",
        405 => "WC202",
        408 => "WC216",
        409 => "WC214",
        413 => "WC204",
        422 => "WC206",
        423 => "WC502",
        426 => "WC213",
        429 => "WC401",
        500 => "WC001",
        502 => "WC300",
        503 => "WC215",
        504 => "WC203",
        508 => "WC403",
        _ => "WC200",
    }
}

/// Classify the error string of a failed provider call or agent start.
///
/// Provider errors are strings formed in `agent/provider.rs` and
/// `agent/registry.rs`; this reads their stable parts. Tested against those
/// exact messages below, so a reworded message fails a test rather than
/// silently falling back to the generic code.
pub fn classify_run_error(message: &str) -> &'static str {
    if let Some(code) = code_in(message) {
        return code;
    }
    let m = message;
    if m.contains("is not set") && (m.contains("API_KEY") || m.contains("api key")) {
        "WC301"
    } else if m.contains("which is not declared") {
        "WC302"
    } else if m.contains("request failed") || m.contains("did not answer") || m.contains("error sending request") {
        "WC303"
    } else if m.contains("unreadable body") {
        "WC305"
    } else if m.contains("context provider") {
        "WC311"
    } else if m.contains("token budget") {
        "WC402"
    } else if names_a_status(m) {
        "WC304"
    } else {
        "WC300"
    }
}

/// `"<provider> <status>: <detail>"`, the shape of a provider's error answer.
fn names_a_status(m: &str) -> bool {
    let mut words = m.split_whitespace();
    let _provider = words.next();
    words.next().is_some_and(|w| {
        let digits = w.trim_end_matches(':');
        digits.len() == 3 && digits.chars().all(|c| c.is_ascii_digit())
    })
}

/// The first catalogued `WCnnn` code that appears in `text`, if any. Used to
/// carry a code across a boundary that only passes a string (a tool call's
/// error, which embeds the failed response's JSON body).
pub fn code_in(text: &str) -> Option<&'static str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        if bytes[i] == b'W' && bytes[i + 1] == b'C' && bytes[i + 2..i + 5].iter().all(u8::is_ascii_digit) {
            let after_ok = bytes.get(i + 5).is_none_or(|b| !b.is_ascii_digit());
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if after_ok && before_ok
                && let Some(info) = lookup(&text[i..i + 5]) {
                    return Some(info.code);
                }
        }
        i += 1;
    }
    None
}

/// The whole catalogue as JSON, for Python and for the MCP status resource.
pub fn catalogue_json() -> Value {
    Value::Array(
        CATALOGUE
            .iter()
            .map(|e| {
                json!({
                    "code": e.code,
                    "title": e.title,
                    "category": e.category,
                    "status": e.status,
                    "hint": e.hint,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_well_formed_and_sorted() {
        let mut seen = std::collections::HashSet::new();
        let mut last = "";
        for e in CATALOGUE {
            assert!(e.code.len() == 5 && e.code.starts_with("WC"), "{}", e.code);
            assert!(e.code[2..].chars().all(|c| c.is_ascii_digit()), "{}", e.code);
            assert!(seen.insert(e.code), "duplicate {}", e.code);
            assert!(e.code > last, "{} is out of order", e.code);
            assert!(!e.hint.is_empty() && !e.title.is_empty(), "{}", e.code);
            last = e.code;
        }
    }

    #[test]
    fn every_status_default_is_catalogued() {
        for status in [400, 401, 403, 404, 405, 409, 413, 418, 422, 423, 426, 429, 500, 502, 503, 504, 508] {
            assert!(lookup(code_for_status(status)).is_some(), "{status}");
        }
    }

    /// The single-source-of-truth check: every `WCnnn` literal anywhere in
    /// this crate's or the bindings' sources must be in the catalogue.
    #[test]
    fn every_code_used_in_rust_is_catalogued() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut stack = vec![root.join("webcortex-core/src"), root.join("webcortex-py/src")];
        let mut files = 0;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("source dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files += 1;
                    let text = std::fs::read_to_string(&path).expect("source");
                    for (i, _) in text.match_indices("\"WC") {
                        let code = &text[i + 1..(i + 6).min(text.len())];
                        if code.len() == 5 && code[2..].chars().all(|c| c.is_ascii_digit()) {
                            assert!(lookup(code).is_some(), "{code} used in {} is not catalogued", path.display());
                        }
                    }
                }
            }
        }
        assert!(files > 10);
    }

    #[test]
    fn provider_messages_classify_to_their_codes() {
        assert_eq!(classify_run_error("ANTHROPIC_API_KEY is not set"), "WC301");
        assert_eq!(
            classify_run_error("model \"claude-x\" needs ANTHROPIC_API_KEY, which is not set. For a local model use a prefixed name such as ollama/<model>"),
            "WC301"
        );
        assert_eq!(classify_run_error("model \"gpt-5\" needs OPENAI_API_KEY, which is not set"), "WC301");
        assert_eq!(
            classify_run_error("model \"nope/m\" names provider \"nope\", which is not declared; declare it"),
            "WC302"
        );
        assert_eq!(classify_run_error("ollama request failed: error sending request"), "WC303");
        assert_eq!(classify_run_error("anthropic returned an unreadable body: eof"), "WC305");
        assert_eq!(classify_run_error("anthropic 529 Overloaded: overloaded"), "WC304");
        assert_eq!(classify_run_error("something odd"), "WC300");
    }

    #[test]
    fn a_code_is_found_inside_an_embedded_body() {
        let text = r#"tool add failed with 422: {"error":{"code":"WC206","message":"x"}}"#;
        assert_eq!(code_in(text), Some("WC206"));
        assert_eq!(code_in("XWC206 and WC2061"), None);
        assert_eq!(code_in(&format!("WC{}", 999)), None);
    }
}
