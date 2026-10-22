//! Model providers.
//!
//! Deliberately *not* a universal LLM abstraction. There are exactly two wire
//! formats here — Anthropic Messages and OpenAI Chat Completions — each
//! implemented properly, plus a trait narrow enough that the runtime never has
//! to know which one it is talking to. The OpenAI format is what Ollama, vLLM,
//! LM Studio, Groq, OpenRouter and OpenAI itself all speak, which is how one
//! implementation covers local models and most hosted ones.
//!
//! The canonical conversation shape is Anthropic's content-block form (text,
//! `tool_use`, `tool_result`). The OpenAI provider translates at its boundary in
//! both directions, so the runtime, the audit trail and the session store see
//! one representation regardless of provider.

use super::{Conversation, ToolSpec};
use crate::manifest::AgentDef;
use futures::future::BoxFuture;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens served from the provider's prompt cache. Billed at a fraction of
    /// the input rate, so they are tracked separately.
    pub cache_read_tokens: u64,
    /// Tokens written to the prompt cache this call.
    pub cache_write_tokens: u64,
    /// The concrete model that answered, after alias resolution.
    pub model: String,
    /// The assistant turn exactly as the provider returned it, so it can be
    /// replayed verbatim in the next request without lossy reconstruction.
    pub raw_content: Value,
}

impl ProviderResponse {
    /// Every token the call spent, cached or not: what a budget is charged.
    /// Saturating, because the counts are whatever the upstream reported.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }
}

/// Everything a provider needs for one completion.
pub struct CompletionRequest<'a> {
    pub agent: &'a AgentDef,
    pub conversation: &'a Conversation,
    pub tools: &'a [ToolSpec],
    /// Force the model to call this tool. This is how structured output is
    /// made a guarantee rather than a request.
    pub force_tool: Option<&'a str>,
    /// Extra system text appended after the agent's own system prompt: resolved
    /// context providers, handoff instructions.
    pub system_suffix: &'a str,
}

pub trait ModelProvider: Send + Sync + 'static {
    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest<'a>,
    ) -> BoxFuture<'a, Result<ProviderResponse, String>>;

    fn name(&self) -> &'static str;

    /// Where requests go, for `webcortex doctor --online`. `None` for
    /// providers with no network endpoint (the fakes).
    fn base_url(&self) -> Option<&str> {
        None
    }
}

fn full_system(req: &CompletionRequest<'_>) -> String {
    let mut s = req.agent.system.clone();
    if !req.system_suffix.is_empty() {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(req.system_suffix);
    }
    s
}

// ---------------------------------------------------------------------------
// The wire, shared by both formats
// ---------------------------------------------------------------------------

/// Extra attempts after a transient failure: a refused connection, or one of
/// [`RETRY_STATUSES`]. The official SDKs retry the same set twice. A timeout
/// is not retried — the client's timeout is ten minutes, and a third of an
/// hour spent re-asking would outlast any request that is waiting on it.
pub const PROVIDER_RETRIES: u32 = 2;

/// Rate limited, overloaded (529 is Anthropic's), or a gateway that lost its
/// upstream: an answer that is likely to be different a moment later.
pub const RETRY_STATUSES: [u16; 8] = [408, 409, 429, 500, 502, 503, 504, 529];

/// Longest `retry-after` honoured. A longer one is the provider saying "not
/// soon", and the run is better told so now than after a minute of silence.
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to wait before attempt `attempt + 1`.
fn retry_delay(attempt: u32, retry_after: Option<std::time::Duration>) -> std::time::Duration {
    retry_after
        .filter(|d| *d <= MAX_RETRY_AFTER)
        .unwrap_or_else(|| std::time::Duration::from_millis(250u64 << attempt.min(6)))
}

/// Send one completion request and return its JSON body, retrying transient
/// failures. Errors keep the shapes the error classifier reads:
/// `"<label> request failed: …"` (WC303), `"<label> <status>: <detail>"`
/// (WC304) and `"<label> returned an unreadable body: …"` (WC305).
///
/// The body is read as bytes before it is parsed, so an error status survives
/// a body that is not JSON: a 429 or 502 from a proxy answers in text or HTML,
/// and parsing first reported it as an unreadable body, losing the status.
async fn send_with_retries(label: &str, request: reqwest::RequestBuilder) -> Result<Value, String> {
    let mut attempt = 0u32;
    loop {
        let Some(this) = request.try_clone() else {
            return Err(format!("{label} request failed: the request body could not be replayed"));
        };
        let res = match this.send().await {
            Ok(res) => res,
            Err(e) if e.is_connect() && attempt < PROVIDER_RETRIES => {
                tokio::time::sleep(retry_delay(attempt, None)).await;
                attempt += 1;
                continue;
            }
            Err(e) => return Err(format!("{label} request failed: {e}")),
        };
        let status = res.status();
        let retry_after = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(std::time::Duration::from_secs_f64);
        let body = res
            .bytes()
            .await
            .map_err(|e| format!("{label} returned an unreadable body: {e}"))?;
        if !status.is_success() {
            if RETRY_STATUSES.contains(&status.as_u16()) && attempt < PROVIDER_RETRIES {
                tracing::debug!(provider = label, %status, attempt, "retrying a transient provider failure");
                tokio::time::sleep(retry_delay(attempt, retry_after)).await;
                attempt += 1;
                continue;
            }
            return Err(format!("{label} {status}: {}", error_detail(&body)));
        }
        return serde_json::from_slice(&body).map_err(|e| {
            format!("{label} returned an unreadable body: {e} (it began {:?})", snippet(&body))
        });
    }
}

/// What an error response says went wrong: `error.message` (both formats),
/// a bare `error` string, or the start of a body that is not JSON at all.
fn error_detail(body: &[u8]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(payload) => match payload.get("error") {
            Some(e) => e
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .or_else(|| e.as_str().map(str::to_string))
                .unwrap_or_else(|| e.to_string()),
            None => payload
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| "no detail".into()),
        },
        Err(_) => {
            let text = snippet(body);
            if text.is_empty() { "no detail".into() } else { text }
        }
    }
}

/// The first couple of hundred characters of a body, whitespace collapsed.
fn snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut out: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 200 {
        out = out.chars().take(200).collect::<String>() + "…";
    }
    out
}

/// A token count from a `usage` field. Some OpenAI-compatible servers report
/// them as floats (`12.0`); reading only integers charged those calls nothing.
fn token_count(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or_else(|| match n.as_f64() {
            Some(f) if f.is_finite() && f > 0.0 => f.round().min(u64::MAX as f64) as u64,
            _ => 0,
        }),
        _ => 0,
    }
}

/// A tool-call id the rest of the turn can rely on: present, non-empty and
/// unique within the turn. A server that omits ids, sends `""`, or repeats one
/// left `tool_result`s that could not be matched to their calls.
fn call_id(given: Option<&str>, seen: &mut std::collections::HashSet<String>) -> String {
    let id = match given.map(str::trim) {
        Some(id) if !id.is_empty() && !seen.contains(id) => id.to_string(),
        _ => format!("call_{}", uuid::Uuid::new_v4().simple()),
    };
    seen.insert(id.clone());
    id
}

/// What a tool call's arguments are taken to be. Anything that is not a JSON
/// object is kept as it came — the runtime refuses such a call (WC317) rather
/// than run the tool with no arguments, which is what a truncated or garbled
/// argument string used to do.
fn call_arguments(raw: Option<&Value>) -> Value {
    match raw {
        None | Some(Value::Null) => json!({}),
        Some(Value::String(s)) if s.trim().is_empty() => json!({}),
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone())),
        Some(v) => v.clone(),
    }
}

/// The `input` a canonical `tool_use` block replays with. Anthropic accepts
/// only an object there; a malformed call is replayed as `{}` and answered
/// with an error result.
fn replay_input(arguments: &Value) -> Value {
    if arguments.is_object() { arguments.clone() } else { json!({}) }
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    version: String,
}

impl AnthropicProvider {
    /// Build from the environment. Returns `None` when no key is configured, so
    /// an app with declared-but-unused agents still boots.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty())?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        Self::new(api_key, base_url)
    }

    pub fn new(api_key: String, base_url: String) -> Option<Self> {
        Some(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .ok()?,
            api_key,
            base_url,
            version: "2023-06-01".into(),
        })
    }
}

impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn base_url(&self) -> Option<&str> {
        Some(&self.base_url)
    }

    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest<'a>,
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            let agent = req.agent;
            let body = anthropic_body(req);

            let request = self
                .client
                .post(format!("{}/v1/messages", self.base_url.trim_end_matches('/')))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", &self.version)
                .header("content-type", "application/json")
                .json(&body);
            let payload = send_with_retries("anthropic", request).await?;
            parse_anthropic(&payload, &agent.model)
        })
    }
}

/// The Messages API request body for one completion.
pub fn anthropic_body(req: &CompletionRequest<'_>) -> Value {
    let agent = req.agent;
    let messages: Vec<Value> = req
        .conversation
        .messages
        .iter()
        .map(|m| json!({"role": m.role, "content": m.content}))
        .collect();

    let mut body = json!({
        "model": agent.model,
        "max_tokens": agent.max_tokens,
        "messages": messages,
    });
    // Only when the app set one: Claude Opus 5, Opus 4.7/4.8 and Sonnet 5
    // reject sampling parameters outright.
    if let Some(t) = agent.temperature {
        body["temperature"] = json!(t);
    }

    let system = full_system(req);
    if !system.is_empty() {
        // Block form so a cache breakpoint can sit on it. The system prompt
        // and tool list are identical on every step of a run, which is exactly
        // the shape prompt caching rewards.
        let mut block = json!({"type": "text", "text": system});
        if agent.cache {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
        body["system"] = json!([block]);
    }
    if !req.tools.is_empty() {
        let n = req.tools.len();
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut tool = json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    });
                    if agent.cache && i + 1 == n {
                        tool["cache_control"] = json!({"type": "ephemeral"});
                    }
                    tool
                })
                .collect(),
        );
        if let Some(name) = req.force_tool {
            body["tool_choice"] = json!({"type": "tool", "name": name});
        }
    }
    body
}

pub fn parse_anthropic(payload: &Value, model: &str) -> Result<ProviderResponse, String> {
    let content = payload
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or("anthropic returned an unreadable body: the response had no content array")?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut raw = Vec::with_capacity(content.len());
    let mut seen = std::collections::HashSet::new();

    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
                raw.push(block.clone());
            }
            Some("tool_use") => {
                let id = call_id(block.get("id").and_then(|i| i.as_str()), &mut seen);
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
                let arguments = call_arguments(block.get("input"));
                // Replayed with the id and input the rest of the turn uses, so
                // a repaired id still pairs with its result.
                let mut replay = block.clone();
                replay["id"] = json!(id);
                replay["input"] = replay_input(&arguments);
                raw.push(replay);
                tool_calls.push(ToolCall { id, name, arguments });
            }
            _ => raw.push(block.clone()),
        }
    }

    let stop_reason = match payload.get("stop_reason").and_then(|s| s.as_str()) {
        Some("tool_use") => StopReason::ToolUse,
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };

    let usage = payload.get("usage");
    let get = |k: &str| token_count(usage.and_then(|u| u.get(k)));
    Ok(ProviderResponse {
        text,
        tool_calls,
        stop_reason,
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_tokens: get("cache_read_input_tokens"),
        cache_write_tokens: get("cache_creation_input_tokens"),
        model: payload
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(model)
            .to_string(),
        raw_content: Value::Array(raw),
    })
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions — Ollama, vLLM, LM Studio, OpenAI, Groq, OpenRouter
// ---------------------------------------------------------------------------

pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
    label: &'static str,
}

impl OpenAiCompatProvider {
    pub fn new(base_url: String, api_key: Option<String>, label: &'static str) -> Option<Self> {
        Some(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .ok()?,
            api_key,
            base_url,
            label,
        })
    }
}

impl ModelProvider for OpenAiCompatProvider {
    fn name(&self) -> &'static str {
        self.label
    }

    fn base_url(&self) -> Option<&str> {
        Some(&self.base_url)
    }

    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest<'a>,
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            let agent = req.agent;
            let body = openai_body(req);

            let mut builder = self
                .client
                .post(format!("{}/chat/completions", self.base_url.trim_end_matches('/')))
                .header("content-type", "application/json");
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
            }
            let payload = send_with_retries(self.label, builder.json(&body)).await?;
            parse_openai(&payload, &agent.model)
        })
    }
}

/// The Chat Completions request for one step.
pub fn openai_body(req: &CompletionRequest) -> Value {
    let agent = req.agent;
    let mut messages: Vec<Value> = Vec::new();
    let system = full_system(req);
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    for m in &req.conversation.messages {
        messages.extend(to_openai_messages(&m.role, &m.content));
    }

    let mut body = json!({
        "model": agent.model,
        "max_tokens": agent.max_tokens,
        "messages": messages,
    });
    if let Some(t) = agent.temperature {
        body["temperature"] = json!(t);
    }
    // Only when the app set it. `none` is how a local thinking model (Qwen,
    // Gemma 4, MiniCPM through Ollama) is told to answer instead of deliberate.
    if let Some(r) = &agent.reasoning {
        body["reasoning_effort"] = json!(r);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect(),
        );
        if let Some(name) = req.force_tool {
            body["tool_choice"] = json!({"type": "function", "function": {"name": name}});
        }
    }
    body
}

/// Translate one canonical (Anthropic-shaped) message into OpenAI messages.
/// A user turn holding tool results becomes one `tool` message per result.
///
/// OpenAI `tool` messages carry text only, so images a tool returned travel in
/// one `user` message after the tool messages, labelled with the call they
/// came from. Images sent with the input become `image_url` parts.
pub fn to_openai_messages(role: &str, content: &Value) -> Vec<Value> {
    match (role, content) {
        (_, Value::String(s)) => vec![json!({"role": role, "content": s})],
        ("user", Value::Array(blocks)) => {
            let mut out = Vec::new();
            let mut text = String::new();
            let mut parts: Vec<Value> = Vec::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => {
                        let id = b.get("tool_use_id").cloned().unwrap_or(Value::Null);
                        let (body, images) = match b.get("content") {
                            Some(Value::String(s)) => (s.clone(), Vec::new()),
                            Some(Value::Array(inner)) => (
                                crate::agent::message_text(&Value::Array(inner.clone())),
                                inner.iter().filter_map(super::vision::openai_part).collect(),
                            ),
                            Some(other) => (other.to_string(), Vec::new()),
                            None => (String::new(), Vec::new()),
                        };
                        if !images.is_empty() {
                            parts.push(json!({"type": "text", "text": format!(
                                "Images returned by tool call {}:",
                                id.as_str().unwrap_or("?")
                            )}));
                            parts.extend(images);
                        }
                        out.push(json!({"role": "tool", "tool_call_id": id, "content": body}));
                    }
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    Some("image") => parts.extend(super::vision::openai_part(b)),
                    _ => {}
                }
            }
            if parts.is_empty() {
                if !text.is_empty() {
                    out.push(json!({"role": "user", "content": text}));
                }
            } else {
                if !text.is_empty() {
                    parts.push(json!({"type": "text", "text": text}));
                }
                out.push(json!({"role": "user", "content": parts}));
            }
            out
        }
        ("assistant", Value::Array(blocks)) => {
            let mut text = String::new();
            let mut calls = Vec::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    Some("tool_use") => calls.push(json!({
                        "id": b.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {
                            "name": b.get("name").cloned().unwrap_or(Value::Null),
                            "arguments": b.get("input")
                                .map(|i| i.to_string())
                                .unwrap_or_else(|| "{}".into()),
                        }
                    })),
                    _ => {}
                }
            }
            let mut m = json!({"role": "assistant"});
            m["content"] = if text.is_empty() { Value::Null } else { json!(text) };
            if !calls.is_empty() {
                m["tool_calls"] = Value::Array(calls);
            }
            vec![m]
        }
        (_, other) => vec![json!({"role": role, "content": other.to_string()})],
    }
}

pub fn parse_openai(payload: &Value, model: &str) -> Result<ProviderResponse, String> {
    // OpenRouter and some proxies report a failure in a 200 body.
    if payload.get("choices").is_none()
        && let Some(e) = payload.get("error")
    {
        return Err(format!("the provider returned an error: {}", error_detail(json!({"error": e}).to_string().as_bytes())));
    }
    let choice = payload
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or("the provider returned an unreadable body: the chat completion had no choices")?;
    let message = choice
        .get("message")
        .filter(|m| m.is_object())
        .ok_or("the provider returned an unreadable body: the chat completion choice had no message")?;

    let text = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };

    let mut tool_calls = Vec::new();
    let mut raw = Vec::new();
    if !text.is_empty() {
        raw.push(json!({"type": "text", "text": text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
        let mut seen = std::collections::HashSet::new();
        for c in calls {
            let id = call_id(c.get("id").and_then(|i| i.as_str()), &mut seen);
            let f = c.get("function").cloned().unwrap_or(Value::Null);
            let name = f
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let arguments = call_arguments(f.get("arguments"));
            raw.push(json!({"type": "tool_use", "id": id, "name": name, "input": replay_input(&arguments)}));
            tool_calls.push(ToolCall { id, name, arguments });
        }
    }

    let stop_reason = match choice.get("finish_reason").and_then(|s| s.as_str()) {
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        // Ollama, vLLM and others answer `stop` for a turn that holds tool
        // calls. Read as the end of the turn, the calls were never run and the
        // run "completed" with no output.
        _ if !tool_calls.is_empty() => StopReason::ToolUse,
        Some("stop") => StopReason::EndTurn,
        _ => StopReason::Other,
    };

    let usage = payload.get("usage");
    let get = |k: &str| token_count(usage.and_then(|u| u.get(k)));
    Ok(ProviderResponse {
        text,
        tool_calls,
        stop_reason,
        input_tokens: get("prompt_tokens"),
        output_tokens: get("completion_tokens"),
        cache_read_tokens: token_count(
            usage.and_then(|u| u.get("prompt_tokens_details")).and_then(|d| d.get("cached_tokens")),
        ),
        cache_write_tokens: 0,
        model: payload
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(model)
            .to_string(),
        raw_content: Value::Array(raw),
    })
}

/// Best-effort recovery of structured output from prose.
///
/// A forced tool call is honoured by hosted providers, but smaller local models
/// frequently answer in text — often *correct* JSON, sometimes wrapped in a
/// code fence. Accepting that costs nothing and turns a class of "model did not
/// return structured output" failures into successes.
pub fn recover_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && (v.is_object() || v.is_array()) {
            return Some(v);
        }
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim);
    if let Some(inner) = unfenced
        && let Ok(v) = serde_json::from_str::<Value>(inner) {
            return Some(v);
        }
    // The first balanced `{ … }` in the text.
    let start = trimmed.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, ch) in trimmed[start..].char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&trimmed[start..start + i + 1]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Scripted provider, for Rust unit tests
// ---------------------------------------------------------------------------

/// Replays a fixed script. Lets the whole agent runtime — budgets, gates, scope
/// delegation — be tested deterministically without a network or an API key.
pub struct ScriptedProvider {
    turns: Vec<Turn>,
    calls: std::sync::atomic::AtomicUsize,
    input_tokens: u64,
    output_tokens: u64,
    repeat_last: bool,
    seen: std::sync::Mutex<Vec<CompletionSnapshot>>,
}

/// What a scripted provider was asked, recorded for assertions.
#[derive(Debug, Clone)]
pub struct CompletionSnapshot {
    pub model: String,
    pub system: String,
    pub system_suffix: String,
    pub message_count: usize,
    pub tool_names: Vec<String>,
    pub force_tool: Option<String>,
    pub messages: Vec<Value>,
}

#[derive(Clone)]
enum Turn {
    Text(String),
    Tool { name: String, arguments: Value },
    Tools(Vec<(String, Value)>),
    Error(String),
    /// Text cut off at `max_tokens`.
    Truncated(String),
    /// A response exactly as given, for wire shapes the others cannot express.
    Response(Box<ProviderResponse>),
}

impl ScriptedProvider {
    fn new(turns: Vec<Turn>, repeat_last: bool) -> Self {
        Self {
            turns,
            calls: std::sync::atomic::AtomicUsize::new(0),
            input_tokens: 10,
            output_tokens: 10,
            repeat_last,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn text(s: &str) -> Self {
        Self::new(vec![Turn::Text(s.into())], false)
    }

    pub fn error(s: &str) -> Self {
        Self::new(vec![Turn::Error(s.into())], true)
    }

    pub fn tool_then_text(tool: &str, arguments: Value, then: &str) -> Self {
        Self::new(
            vec![
                Turn::Tool { name: tool.into(), arguments },
                Turn::Text(then.into()),
            ],
            false,
        )
    }

    /// Several tool calls in one turn, then a text answer.
    pub fn tools_then_text(tools: Vec<(&str, Value)>, then: &str) -> Self {
        Self::new(
            vec![
                Turn::Tools(tools.into_iter().map(|(n, a)| (n.to_string(), a)).collect()),
                Turn::Text(then.into()),
            ],
            false,
        )
    }

    /// An arbitrary sequence: each entry is `("tool", name, args)` or
    /// `("text", s, _)`.
    pub fn sequence(turns: Vec<(&str, &str, Value)>) -> Self {
        Self::new(
            turns
                .into_iter()
                .map(|(kind, a, b)| match kind {
                    "tool" => Turn::Tool { name: a.into(), arguments: b },
                    "error" => Turn::Error(a.into()),
                    "truncated" => Turn::Truncated(a.into()),
                    _ => Turn::Text(a.into()),
                })
                .collect(),
            false,
        )
    }

    /// Replay these responses verbatim, in order.
    pub fn responses(responses: Vec<ProviderResponse>) -> Self {
        Self::new(responses.into_iter().map(|r| Turn::Response(Box::new(r))).collect(), false)
    }

    pub fn always_tool(tool: &str, arguments: Value) -> Self {
        Self::new(vec![Turn::Tool { name: tool.into(), arguments }], true)
    }

    pub fn with_tokens(mut self, input: u64, output: u64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }

    pub fn snapshots(&self) -> Vec<CompletionSnapshot> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest<'a>,
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(CompletionSnapshot {
                    model: req.agent.model.clone(),
                    system: req.agent.system.clone(),
                    system_suffix: req.system_suffix.to_string(),
                    message_count: req.conversation.messages.len(),
                    tool_names: req.tools.iter().map(|t| t.name.clone()).collect(),
                    force_tool: req.force_tool.map(str::to_string),
                    messages: req
                        .conversation
                        .messages
                        .iter()
                        .map(|m| json!({"role": m.role, "content": m.content}))
                        .collect(),
                });
            }
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let turn = match self.turns.get(n) {
                Some(t) => t.clone(),
                None if self.repeat_last => {
                    self.turns.last().cloned().unwrap_or(Turn::Text(String::new()))
                }
                None => Turn::Text(String::new()),
            };

            let model = req.agent.model.clone();
            match turn {
                Turn::Error(e) => Err(e),
                Turn::Response(r) => Ok(*r),
                Turn::Truncated(text) => Ok(ProviderResponse {
                    raw_content: json!([{"type": "text", "text": text}]),
                    text,
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::MaxTokens,
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    model,
                }),
                Turn::Text(text) => Ok(ProviderResponse {
                    raw_content: json!([{"type": "text", "text": text}]),
                    text,
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    model,
                }),
                Turn::Tool { name, arguments } => {
                    let id = format!("call_{n}");
                    Ok(ProviderResponse {
                        raw_content: json!([{
                            "type": "tool_use", "id": id,
                            "name": name, "input": arguments
                        }]),
                        text: String::new(),
                        tool_calls: vec![ToolCall { id, name, arguments }],
                        stop_reason: StopReason::ToolUse,
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                        model,
                    })
                }
                Turn::Tools(list) => {
                    let mut raw = Vec::new();
                    let mut calls = Vec::new();
                    for (i, (name, arguments)) in list.into_iter().enumerate() {
                        let id = format!("call_{n}_{i}");
                        raw.push(json!({"type": "tool_use", "id": id, "name": name, "input": arguments}));
                        calls.push(ToolCall { id, name, arguments });
                    }
                    Ok(ProviderResponse {
                        raw_content: Value::Array(raw),
                        text: String::new(),
                        tool_calls: calls,
                        stop_reason: StopReason::ToolUse,
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                        model,
                    })
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Fake provider, for offline end-to-end tests over HTTP
// ---------------------------------------------------------------------------

/// A deterministic stand-in for a model, driven by a tiny command language in
/// the latest user message. Enabled with `WEBCORTEX_FAKE_PROVIDER=1`; never
/// selected otherwise. It exists so the *whole* stack — HTTP, sessions,
/// handoffs, approval resume, budgets — can be exercised by the Python test
/// suite without a network or a key.
///
/// Commands, one per `;`-separated clause of the last user text:
///
/// * `tool:<name> <json>`   — request that tool call
/// * `handoff:<agent>`      — request `transfer_to_<agent>`
/// * `json:<object>`        — when a tool is being forced, answer with it
/// * `context?`             — answer with the resolved system suffix, so a
///   test can see what context providers injected
/// * anything else          — answered as text, prefixed by the agent name
///
/// After tool results come back the fake answers with a text summary of them,
/// so a run terminates.
pub struct FakeProvider;

impl ModelProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest<'a>,
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            let model = req.agent.model.clone();
            // Rough but monotone: the more conversation, the more input tokens,
            // which is what makes compaction observable.
            let input_tokens: u64 = req
                .conversation
                .messages
                .iter()
                .map(|m| m.content.to_string().len() as u64 / 4 + 1)
                .sum::<u64>()
                + req.system_suffix.len() as u64 / 4;
            let done = |text: String, calls: Vec<ToolCall>, raw: Value, stop: StopReason| {
                Ok(ProviderResponse {
                    text,
                    tool_calls: calls,
                    stop_reason: stop,
                    input_tokens,
                    output_tokens: 10,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    model: model.clone(),
                    raw_content: raw,
                })
            };

            let last = req.conversation.messages.last();
            let is_tool_results = |c: &Value| {
                c.as_array().is_some_and(|bs| {
                    bs.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                })
            };
            let last_text = match last.map(|m| &m.content) {
                Some(Value::String(s)) => Some(s.clone()),
                // Input sent with images: the text block is the prompt.
                Some(c @ Value::Array(_)) if !is_tool_results(c) => Some(crate::agent::message_text(c)),
                _ => None,
            };
            // So a test can see that pixels reached the model.
            let seen = last.map(|m| super::vision::count(&m.content)).unwrap_or(0);
            let images_note = if seen > 0 { format!(" [saw {seen} image(s)]") } else { String::new() };

            // A forced tool: structured output.
            if let Some(forced) = req.force_tool {
                let prompt = last_text.clone().unwrap_or_default();
                // The first JSON value after `json:`, ignoring whatever follows
                // it — a flow's classifier appends the input after the prompt.
                let args = prompt
                    .find("json:")
                    .and_then(|i| {
                        serde_json::Deserializer::from_str(prompt[i + 5..].trim())
                            .into_iter::<Value>()
                            .next()
                            .and_then(|v| v.ok())
                    })
                    .unwrap_or_else(|| json!({"text": prompt, "agent": req.agent.name}));
                let id = "fake_forced".to_string();
                return done(
                    String::new(),
                    vec![ToolCall { id: id.clone(), name: forced.into(), arguments: args.clone() }],
                    json!([{"type": "tool_use", "id": id, "name": forced, "input": args}]),
                    StopReason::ToolUse,
                );
            }

            let Some(text) = last_text else {
                // Tool results just came back: summarise them and stop.
                // The tail of each result, plus its length: the tail is where
                // a truncation marker lives, and the length is how a test can
                // tell that bounding happened.
                let summary = match last.map(|m| &m.content) {
                    Some(Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|b| b.get("content"))
                        .map(|c| c.as_str().map(str::to_string).unwrap_or_else(|| crate::agent::message_text(c)))
                        .map(|s| {
                            let n = s.chars().count();
                            let tail: String = s.chars().skip(n.saturating_sub(160)).collect();
                            format!("{tail} (len={n})")
                        })
                        .collect::<Vec<_>>()
                        .join(" | "),
                    _ => String::new(),
                };

                let reply = format!("{} says: {summary}{images_note}", req.agent.name);
                return done(
                    reply.clone(),
                    Vec::new(),
                    json!([{"type": "text", "text": reply}]),
                    StopReason::EndTurn,
                );
            };

            if text.trim() == "context?" {
                let reply = format!("{}|{}", req.agent.system, req.system_suffix);
                return done(
                    reply.clone(),
                    Vec::new(),
                    json!([{"type": "text", "text": reply}]),
                    StopReason::EndTurn,
                );
            }

            // `fail:<message>` answers as a provider error would, so the failure
            // path (codes, hints, the status view) is testable offline. Write
            // the message the way a real provider error reads, e.g.
            // `fail:anthropic 529: overloaded`.
            if let Some(message) = text.trim().strip_prefix("fail:") {
                return Err(message.trim().to_string());
            }

            let mut calls = Vec::new();
            let mut raw = Vec::new();
            for (i, clause) in text.split(';').map(str::trim).enumerate() {

                let (name, args) = if let Some(rest) = clause.strip_prefix("tool:") {
                    let (name, args) = rest.trim().split_once(' ').unwrap_or((rest.trim(), "{}"));
                    (name.to_string(), serde_json::from_str(args.trim()).unwrap_or_else(|_| json!({})))
                } else if let Some(agent) = clause.strip_prefix("handoff:") {
                    (format!("transfer_to_{}", agent.trim()), json!({"reason": "asked to"}))
                } else {
                    continue;
                };
                let id = format!("fake_{i}");
                raw.push(json!({"type": "tool_use", "id": id, "name": name, "input": args}));
                calls.push(ToolCall { id, name, arguments: args });
            }
            if !calls.is_empty() {
                return done(String::new(), calls, Value::Array(raw), StopReason::ToolUse);
            }

            let reply = format!("{} echoes: {text}{images_note}", req.agent.name);
            done(
                reply.clone(),
                Vec::new(),
                json!([{"type": "text", "text": reply}]),
                StopReason::EndTurn,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_is_sent_only_when_the_app_set_one() {
        let mut agent: AgentDef =
            serde_json::from_value(json!({"name": "a", "model": "claude-opus-5"})).unwrap();
        let conversation = Conversation::default();
        let body = |agent: &AgentDef| {
            anthropic_body(&CompletionRequest {
                agent,
                conversation: &conversation,
                tools: &[],
                force_tool: None,
                system_suffix: "",
            })
        };
        assert!(body(&agent).get("temperature").is_none(), "Opus 5 rejects any sampling parameter");
        agent.temperature = Some(0.2);
        assert_eq!(body(&agent)["temperature"], json!(0.2f32));
    }

    #[test]
    fn reasoning_is_sent_as_reasoning_effort_only_on_the_openai_format() {
        let mut agent: AgentDef =
            serde_json::from_value(json!({"name": "a", "model": "ollama/qwen3.5:4b"})).unwrap();
        let conversation = Conversation::default();
        fn request<'a>(agent: &'a AgentDef, conversation: &'a Conversation) -> CompletionRequest<'a> {
            CompletionRequest { agent, conversation, tools: &[], force_tool: None, system_suffix: "" }
        }
        assert!(openai_body(&request(&agent, &conversation)).get("reasoning_effort").is_none());
        agent.reasoning = Some("none".into());
        assert_eq!(openai_body(&request(&agent, &conversation))["reasoning_effort"], json!("none"));
        assert!(
            anthropic_body(&request(&agent, &conversation)).get("reasoning_effort").is_none(),
            "the Anthropic format has no such field"
        );
    }

    #[test]
    fn parses_a_text_response() {
        let payload = json!({
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 7,
                      "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2}
        });
        let r = parse_anthropic(&payload, "m").expect("parses");
        assert_eq!(r.text, "hello");
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert_eq!((r.input_tokens, r.output_tokens), (5, 7));
        assert_eq!((r.cache_read_tokens, r.cache_write_tokens), (3, 2));
    }

    #[test]
    fn parses_tool_use_including_mixed_content() {
        let payload = json!({
            "content": [
                {"type": "text", "text": "let me check"},
                {"type": "tool_use", "id": "t1", "name": "lookup", "input": {"q": "x"}}
            ],
            "stop_reason": "tool_use"
        });
        let r = parse_anthropic(&payload, "m").expect("parses");
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.text, "let me check");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "lookup");
        assert_eq!(r.tool_calls[0].arguments, json!({"q": "x"}));
    }

    #[test]
    fn raw_content_round_trips_for_replay() {
        let content = json!([{"type": "text", "text": "hi"}]);
        let r = parse_anthropic(&json!({"content": content, "stop_reason": "end_turn"}), "m")
            .expect("parses");
        assert_eq!(r.raw_content, content);
    }

    #[test]
    fn a_missing_content_array_is_an_error_not_a_panic() {
        assert!(parse_anthropic(&json!({"stop_reason": "end_turn"}), "m").is_err());
    }

    #[test]
    fn max_tokens_is_distinguished_from_a_normal_stop() {
        let r = parse_anthropic(&json!({"content": [], "stop_reason": "max_tokens"}), "m")
            .expect("parses");
        assert_eq!(r.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn openai_tool_calls_parse_and_arguments_are_decoded_from_their_string_form() {
        let payload = json!({
            "model": "qwen3.5:9b",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {"name": "list_books", "arguments": "{\"limit\": 5}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 40, "completion_tokens": 9,
                      "prompt_tokens_details": {"cached_tokens": 30}}
        });
        let r = parse_openai(&payload, "ollama/qwen3.5:9b").expect("parses");
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.tool_calls[0].name, "list_books");
        assert_eq!(r.tool_calls[0].arguments, json!({"limit": 5}));
        assert_eq!(r.cache_read_tokens, 30);
        assert_eq!(r.model, "qwen3.5:9b");
        // The raw content is canonical Anthropic-shaped, so the conversation
        // store never sees provider-specific structure.
        assert_eq!(r.raw_content[0]["type"], "tool_use");
    }

    #[test]
    fn openai_text_answer_parses() {
        let payload = json!({
            "choices": [{"finish_reason": "stop",
                         "message": {"role": "assistant", "content": "hello"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let r = parse_openai(&payload, "m").expect("parses");
        assert_eq!(r.text, "hello");
        assert_eq!(r.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn canonical_tool_results_become_openai_tool_messages() {
        let content = json!([
            {"type": "tool_result", "tool_use_id": "c1", "content": "{\"ok\":true}", "is_error": false},
            {"type": "tool_result", "tool_use_id": "c2", "content": "{\"n\":2}", "is_error": false}
        ]);
        let out = to_openai_messages("user", &content);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "c1");
        assert_eq!(out[1]["content"], "{\"n\":2}");
    }

    #[test]
    fn images_reach_openai_as_image_url_parts_after_the_tool_messages() {
        let img = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}});
        let content = json!([
            {"type": "tool_result", "tool_use_id": "c1", "is_error": false,
             "content": [{"type": "text", "text": "{\"frame\":\"[image 1]\"}"}, img]},
        ]);
        let out = to_openai_messages("user", &content);
        assert_eq!(out.len(), 2, "tool message, then the images");
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["content"], "{\"frame\":\"[image 1]\"}");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"][1]["image_url"]["url"], "data:image/png;base64,AAAA");

        let input = json!([img, {"type": "text", "text": "what is this?"}]);
        let out = to_openai_messages("user", &input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"][0]["type"], "image_url");
        assert_eq!(out[0]["content"][1]["text"], "what is this?");
    }

    #[test]
    fn canonical_assistant_tool_use_becomes_openai_tool_calls() {
        let content = json!([
            {"type": "text", "text": "checking"},
            {"type": "tool_use", "id": "c1", "name": "lookup", "input": {"q": 1}}
        ]);
        let out = to_openai_messages("assistant", &content);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], "checking");
        assert_eq!(out[0]["tool_calls"][0]["function"]["name"], "lookup");
        assert_eq!(out[0]["tool_calls"][0]["function"]["arguments"], "{\"q\":1}");
    }

    #[test]
    fn openai_tool_calls_under_a_stop_finish_are_still_tool_use() {
        // Ollama and vLLM answer `finish_reason: "stop"` for a turn of calls.
        let payload = json!({"choices": [{"finish_reason": "stop", "message": {"content": null,
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]}}]});
        assert_eq!(parse_openai(&payload, "m").unwrap().stop_reason, StopReason::ToolUse);
        let cut = json!({"choices": [{"finish_reason": "length", "message": {
            "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": "{\"a\":"}}]}}]});
        assert_eq!(parse_openai(&cut, "m").unwrap().stop_reason, StopReason::MaxTokens, "a cut-off turn is never run");
    }

    #[test]
    fn tool_call_ids_are_present_and_unique_within_a_turn() {
        let payload = json!({"choices": [{"finish_reason": "tool_calls", "message": {"tool_calls": [
            {"function": {"name": "f", "arguments": "{}"}},
            {"id": "", "function": {"name": "f", "arguments": "{}"}},
            {"id": "dup", "function": {"name": "f", "arguments": "{}"}},
            {"id": "dup", "function": {"name": "f", "arguments": "{}"}}
        ]}}]});
        let r = parse_openai(&payload, "m").unwrap();
        let ids: std::collections::HashSet<_> = r.tool_calls.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids.len(), 4, "{:?}", r.tool_calls);
        assert!(ids.iter().all(|i| !i.is_empty()));
        assert_eq!(r.tool_calls[2].id, "dup", "a usable id is kept");
        // The replayed turn carries the same ids, so results pair with calls.
        let replayed: Vec<_> = r.raw_content.as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(replayed, r.tool_calls.iter().map(|c| c.id.clone()).collect::<Vec<_>>());

        let anthropic = json!({"stop_reason": "tool_use", "content": [
            {"type": "tool_use", "name": "f", "input": {}},
            {"type": "tool_use", "id": "", "name": "f", "input": {}}]});
        let r = parse_anthropic(&anthropic, "m").unwrap();
        assert_ne!(r.tool_calls[0].id, r.tool_calls[1].id);
        assert_eq!(r.raw_content[1]["id"], json!(r.tool_calls[1].id));
    }

    #[test]
    fn garbled_arguments_are_kept_as_they_came_and_replayed_as_an_object() {
        let payload = json!({"choices": [{"finish_reason": "tool_calls", "message": {"tool_calls": [
            {"id": "a", "function": {"name": "f", "arguments": "{\"id\": 5, \"conf"}},
            {"id": "b", "function": {"name": "f", "arguments": "[1,2]"}},
            {"id": "c", "function": {"name": "f", "arguments": ""}},
            {"id": "d", "function": {"name": "f", "arguments": {"x": 1}}}
        ]}}]});
        let r = parse_openai(&payload, "m").unwrap();
        assert_eq!(r.tool_calls[0].arguments, json!("{\"id\": 5, \"conf"), "not silently {{}}");
        assert_eq!(r.tool_calls[1].arguments, json!([1, 2]));
        assert_eq!(r.tool_calls[2].arguments, json!({}), "no arguments at all is an empty object");
        assert_eq!(r.tool_calls[3].arguments, json!({"x": 1}));
        assert_eq!(r.raw_content[0]["input"], json!({}), "Anthropic replays only an object");
        let anthropic = json!({"stop_reason": "tool_use", "content": [{"type": "tool_use", "id": "a", "name": "f", "input": "x"}]});
        let r = parse_anthropic(&anthropic, "m").unwrap();
        assert_eq!(r.tool_calls[0].arguments, json!("x"));
        assert_eq!(r.raw_content[0]["input"], json!({}));
    }

    #[test]
    fn usage_reported_as_floats_is_still_counted() {
        let payload = json!({"choices": [{"finish_reason": "stop", "message": {"content": "x"}}],
                             "usage": {"prompt_tokens": 12.0, "completion_tokens": 3.4,
                                       "prompt_tokens_details": {"cached_tokens": -1}}});
        let r = parse_openai(&payload, "m").unwrap();
        assert_eq!((r.input_tokens, r.output_tokens, r.cache_read_tokens), (12, 3, 0));
        let r = parse_anthropic(&json!({"content": [], "usage": {"input_tokens": 1e30, "output_tokens": "9"}}), "m").unwrap();
        assert_eq!((r.input_tokens, r.output_tokens), (u64::MAX, 0), "huge saturates; a string is not a count");
    }

    #[test]
    fn an_error_body_that_is_not_json_still_says_what_happened() {
        assert_eq!(error_detail(br#"{"error": {"message": "overloaded"}}"#), "overloaded");
        assert_eq!(error_detail(br#"{"error": "bad model"}"#), "bad model");
        assert_eq!(error_detail(br#"{"error": {"code": 7}}"#), r#"{"code":7}"#);
        assert_eq!(error_detail(b"<html>\n  <h1>502 Bad Gateway</h1>\n</html>"), "<html> <h1>502 Bad Gateway</h1> </html>");
        assert_eq!(error_detail(b""), "no detail");
        assert!(error_detail(&vec![b'x'; 5000]).chars().count() <= 201);
        // The message keeps the shape the classifier reads as WC304.
        let msg = format!("openai 429 Too Many Requests: {}", error_detail(b"slow down"));
        assert_eq!(crate::errors::classify_run_error(&msg), "WC304");
    }

    #[test]
    fn malformed_success_bodies_are_unreadable_responses_with_their_detail() {
        use crate::errors::classify_run_error as code;
        let e = parse_anthropic(&json!({"type": "message"}), "m").unwrap_err();
        assert_eq!(code(&e), "WC305", "{e}");
        let e = parse_openai(&json!({"choices": []}), "m").unwrap_err();
        assert_eq!(code(&e), "WC305", "{e}");
        let e = parse_openai(&json!({"choices": [{"message": null}]}), "m").unwrap_err();
        assert_eq!(code(&e), "WC305", "{e}");
        let e = parse_openai(&json!({"error": {"message": "credit exhausted"}}), "m").unwrap_err();
        assert!(e.contains("credit exhausted") && code(&e) == "WC304", "{e}");
        let e = parse_openai(&json!({"error": "nope"}), "m").unwrap_err();
        assert!(e.contains("nope"), "{e}");
    }

    #[test]
    fn retries_back_off_and_honour_a_short_retry_after() {
        use std::time::Duration;
        assert_eq!(retry_delay(0, None), Duration::from_millis(250));
        assert_eq!(retry_delay(1, None), Duration::from_millis(500));
        assert_eq!(retry_delay(0, Some(Duration::from_secs(2))), Duration::from_secs(2));
        assert_eq!(retry_delay(0, Some(Duration::from_secs(3600))), Duration::from_millis(250), "a long wait is not honoured");
        assert_eq!(retry_delay(u32::MAX, None), Duration::from_millis(250 << 6), "no shift overflow");
    }

    #[test]
    fn json_is_recovered_from_prose_and_fences() {
        assert_eq!(recover_json(r#"{"a": 1}"#), Some(json!({"a": 1})));
        assert_eq!(recover_json("```json\n{\"a\": 1}\n```"), Some(json!({"a": 1})));
        assert_eq!(
            recover_json("Sure! Here you go: {\"a\": {\"b\": \"}\"}} done"),
            Some(json!({"a": {"b": "}"}}))
        );
        assert_eq!(recover_json("no json here"), None);
        assert_eq!(recover_json("42"), None, "a bare scalar is not structured output");
    }
}
