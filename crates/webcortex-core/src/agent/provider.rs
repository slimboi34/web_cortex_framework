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
                .timeout(std::time::Duration::from_secs(300))
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

    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest<'a>,
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
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
                "temperature": agent.temperature,
                "messages": messages,
            });

            let system = full_system(req);
            if !system.is_empty() {
                // Block form so a cache breakpoint can sit on it. The system
                // prompt and tool list are identical on every step of a run,
                // which is exactly the shape prompt caching rewards.
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

            let res = self
                .client
                .post(format!("{}/v1/messages", self.base_url.trim_end_matches('/')))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", &self.version)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("anthropic request failed: {e}"))?;

            let status = res.status();
            let payload: Value = res
                .json()
                .await
                .map_err(|e| format!("anthropic returned an unreadable body: {e}"))?;

            if !status.is_success() {
                let detail = payload
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("no detail");
                return Err(format!("anthropic {status}: {detail}"));
            }

            parse_anthropic(&payload, &agent.model)
        })
    }
}

pub fn parse_anthropic(payload: &Value, model: &str) -> Result<ProviderResponse, String> {
    let content = payload
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or("anthropic response had no content array")?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => tool_calls.push(ToolCall {
                id: block
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string(),
                arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
            }),
            _ => {}
        }
    }

    let stop_reason = match payload.get("stop_reason").and_then(|s| s.as_str()) {
        Some("tool_use") => StopReason::ToolUse,
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };

    let usage = payload.get("usage");
    let get = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
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
        raw_content: Value::Array(content.clone()),
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

    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest<'a>,
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
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
                "temperature": agent.temperature,
                "messages": messages,
            });
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

            let mut builder = self
                .client
                .post(format!("{}/chat/completions", self.base_url.trim_end_matches('/')))
                .header("content-type", "application/json");
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
            }
            let res = builder
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("{} request failed: {e}", self.label))?;

            let status = res.status();
            let payload: Value = res
                .json()
                .await
                .map_err(|e| format!("{} returned an unreadable body: {e}", self.label))?;

            if !status.is_success() {
                let detail = payload
                    .get("error")
                    .and_then(|e| e.get("message").or(Some(e)))
                    .map(|m| m.as_str().map(str::to_string).unwrap_or_else(|| m.to_string()))
                    .unwrap_or_else(|| "no detail".into());
                return Err(format!("{} {status}: {detail}", self.label));
            }

            parse_openai(&payload, &agent.model)
        })
    }
}

/// Translate one canonical (Anthropic-shaped) message into OpenAI messages.
/// A user turn holding tool results becomes one `tool` message per result.
pub fn to_openai_messages(role: &str, content: &Value) -> Vec<Value> {
    match (role, content) {
        (_, Value::String(s)) => vec![json!({"role": role, "content": s})],
        ("user", Value::Array(blocks)) => {
            let mut out = Vec::new();
            let mut text = String::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => out.push(json!({
                        "role": "tool",
                        "tool_call_id": b.get("tool_use_id").cloned().unwrap_or(Value::Null),
                        "content": match b.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        },
                    })),
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text}));
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
    let choice = payload
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or("chat completion had no choices")?;
    let message = choice.get("message").ok_or("chat completion choice had no message")?;

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
        for (i, c) in calls.iter().enumerate() {
            let id = c
                .get("id")
                .and_then(|i| i.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{i}"));
            let f = c.get("function").cloned().unwrap_or(Value::Null);
            let name = f
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let arguments = match f.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                Some(v) => v.clone(),
                None => json!({}),
            };
            raw.push(json!({"type": "tool_use", "id": id, "name": name, "input": arguments}));
            tool_calls.push(ToolCall { id, name, arguments });
        }
    }

    let stop_reason = match choice.get("finish_reason").and_then(|s| s.as_str()) {
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        _ if !tool_calls.is_empty() => StopReason::ToolUse,
        _ => StopReason::Other,
    };

    let usage = payload.get("usage");
    let get = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    Ok(ProviderResponse {
        text,
        tool_calls,
        stop_reason,
        input_tokens: get("prompt_tokens"),
        output_tokens: get("completion_tokens"),
        cache_read_tokens: usage
            .and_then(|u| u.get("prompt_tokens_details"))
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
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
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() || v.is_array() {
            return Some(v);
        }
    }
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim);
    if let Some(inner) = unfenced {
        if let Ok(v) = serde_json::from_str::<Value>(inner) {
            return Some(v);
        }
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
                    _ => Turn::Text(a.into()),
                })
                .collect(),
            false,
        )
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
            let last_text = match last.map(|m| &m.content) {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            };

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
                        .filter_map(|b| b.get("content").and_then(|c| c.as_str()))
                        .map(|s| {
                            let n = s.chars().count();
                            let tail: String = s.chars().skip(n.saturating_sub(160)).collect();
                            format!("{tail} (len={n})")
                        })
                        .collect::<Vec<_>>()
                        .join(" | "),
                    _ => String::new(),
                };

                let reply = format!("{} says: {summary}", req.agent.name);
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

            let reply = format!("{} echoes: {text}", req.agent.name);
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
