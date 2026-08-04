//! Model providers.
//!
//! Deliberately *not* a universal LLM abstraction. Every vendor's tool-call and
//! streaming format differs and changes; a lowest-common-denominator interface
//! rots quickly and hides the differences that matter. One provider implemented
//! properly, plus a trait narrow enough that adding a second is a contained
//! piece of work.

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
    /// The assistant turn exactly as the provider returned it, so it can be
    /// replayed verbatim in the next request without lossy reconstruction.
    pub raw_content: Value,
}

pub trait ModelProvider: Send + Sync + 'static {
    fn complete<'a>(
        &'a self,
        agent: &'a AgentDef,
        conversation: &'a Conversation,
        tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<ProviderResponse, String>>;

    fn name(&self) -> &'static str;
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
        Some(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .ok()?,
            api_key,
            base_url: std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".into()),
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
        agent: &'a AgentDef,
        conversation: &'a Conversation,
        tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            let messages: Vec<Value> = conversation
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
            if !agent.system.is_empty() {
                body["system"] = json!(agent.system);
            }
            if !tools.is_empty() {
                body["tools"] = Value::Array(
                    tools
                        .iter()
                        .map(|t| {
                            json!({
                                "name": t.name,
                                "description": t.description,
                                "input_schema": t.input_schema,
                            })
                        })
                        .collect(),
                );
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

            parse_anthropic(&payload)
        })
    }
}

fn parse_anthropic(payload: &Value) -> Result<ProviderResponse, String> {
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
    Ok(ProviderResponse {
        text,
        tool_calls,
        stop_reason,
        input_tokens: usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        raw_content: Value::Array(content.clone()),
    })
}

// ---------------------------------------------------------------------------
// Scripted provider, for tests and offline development
// ---------------------------------------------------------------------------

/// Replays a fixed script. Lets the whole agent runtime — budgets, gates, scope
/// delegation — be tested deterministically without a network or an API key.
pub struct ScriptedProvider {
    turns: Vec<Turn>,
    calls: std::sync::atomic::AtomicUsize,
    input_tokens: u64,
    output_tokens: u64,
    repeat_last: bool,
}

#[derive(Clone)]
enum Turn {
    Text(String),
    Tool { name: String, arguments: Value },
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

    pub fn always_tool(tool: &str, arguments: Value) -> Self {
        Self::new(vec![Turn::Tool { name: tool.into(), arguments }], true)
    }

    pub fn with_tokens(mut self, input: u64, output: u64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }
}

impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn complete<'a>(
        &'a self,
        _agent: &'a AgentDef,
        _conversation: &'a Conversation,
        _tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let turn = match self.turns.get(n) {
                Some(t) => t.clone(),
                None if self.repeat_last => {
                    self.turns.last().cloned().unwrap_or(Turn::Text(String::new()))
                }
                None => Turn::Text(String::new()),
            };

            match turn {
                Turn::Error(e) => Err(e),
                Turn::Text(text) => Ok(ProviderResponse {
                    raw_content: json!([{"type": "text", "text": text}]),
                    text,
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
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
                    })
                }
            }
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
            "usage": {"input_tokens": 5, "output_tokens": 7}
        });
        let r = parse_anthropic(&payload).expect("parses");
        assert_eq!(r.text, "hello");
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert_eq!((r.input_tokens, r.output_tokens), (5, 7));
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
        let r = parse_anthropic(&payload).expect("parses");
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.text, "let me check");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "lookup");
        assert_eq!(r.tool_calls[0].arguments, json!({"q": "x"}));
    }

    #[test]
    fn raw_content_round_trips_for_replay() {
        let content = json!([{"type": "text", "text": "hi"}]);
        let r = parse_anthropic(&json!({"content": content, "stop_reason": "end_turn"}))
            .expect("parses");
        assert_eq!(r.raw_content, content);
    }

    #[test]
    fn a_missing_content_array_is_an_error_not_a_panic() {
        assert!(parse_anthropic(&json!({"stop_reason": "end_turn"})).is_err());
    }

    #[test]
    fn max_tokens_is_distinguished_from_a_normal_stop() {
        let r = parse_anthropic(&json!({"content": [], "stop_reason": "max_tokens"}))
            .expect("parses");
        assert_eq!(r.stop_reason, StopReason::MaxTokens);
    }
}
