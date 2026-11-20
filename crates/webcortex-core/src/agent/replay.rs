//! Cassettes: record a run's model responses once, replay them for free.
//!
//! An agent application has two halves. The model's choices are the
//! probabilistic half; everything around them — which tools exist, what scopes
//! a delegate holds, how a handoff narrows authority, where a budget stops a
//! tree, what a flow feeds the next step — is deterministic code and
//! declaration. An eval that calls a live model on every run tests both at
//! once, costs tokens every time, and fails for reasons that are not yours.
//!
//! A cassette separates them. [`RecordingProvider`] wraps the real provider
//! and keeps every response; [`ReplayProvider`] answers from that recording
//! with no network and no key. The tools still run, for real, through the
//! same dispatcher: what is replayed is only the model.
//!
//! **Matching.** Responses are replayed per agent, in the order they were
//! recorded. Each recorded call also carries a *fingerprint* of the request's
//! stable parts — the agent's system prompt, its model name, its tools and
//! their schemas, a forced tool, its request options. If the fingerprint of
//! the call being replayed differs, the cassette is stale: the agent was
//! changed since it was recorded, the recorded answer is an answer to a
//! different question, and the call fails with `WC319` rather than pretend.
//! The conversation itself is not fingerprinted, because tool results carry
//! run ids and timings that differ on every run.

use super::provider::{CompletionRequest, ModelProvider, ProviderResponse, StopReason, ToolCall};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// The cassette format this build reads and writes.
pub const CASSETTE_VERSION: u64 = 1;

/// The stable parts of a completion request, hashed. See the module docs for
/// what is and is not included, and why.
pub fn fingerprint(req: &CompletionRequest<'_>) -> String {
    let a = req.agent;
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!([t.name, t.description, t.input_schema]))
        .collect();
    let canonical = json!([
        a.name, a.model, a.system, tools, req.force_tool, a.options, a.max_tokens,
        a.temperature.map(|t| t.to_string()), a.reasoning,
    ]);
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn stop_name(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::Other => "other",
    }
}

fn response_to_json(r: &ProviderResponse) -> Value {
    json!({
        "text": r.text,
        "tool_calls": r.tool_calls.iter().map(|c| json!({"id": c.id, "name": c.name, "arguments": c.arguments})).collect::<Vec<_>>(),
        "stop_reason": stop_name(&r.stop_reason),
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "cache_read_tokens": r.cache_read_tokens,
        "cache_write_tokens": r.cache_write_tokens,
        "model": r.model,
        "raw_content": r.raw_content,
    })
}

fn response_from_json(v: &Value) -> Result<ProviderResponse, String> {
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let count = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let stop_reason = match v.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("other") => StopReason::Other,
        other => return Err(format!("unknown stop_reason {other:?}")),
    };
    let mut tool_calls = Vec::new();
    for c in v.get("tool_calls").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        tool_calls.push(ToolCall {
            id: c.get("id").and_then(Value::as_str).ok_or("a tool call has no id")?.to_string(),
            name: c.get("name").and_then(Value::as_str).ok_or("a tool call has no name")?.to_string(),
            arguments: c.get("arguments").cloned().unwrap_or(Value::Null),
        });
    }
    Ok(ProviderResponse {
        text: text("text").unwrap_or_default(),
        tool_calls,
        stop_reason,
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        cache_read_tokens: count("cache_read_tokens"),
        cache_write_tokens: count("cache_write_tokens"),
        model: text("model").ok_or("a recorded response has no model")?,
        raw_content: v.get("raw_content").cloned().ok_or("a recorded response has no raw_content")?,
    })
}

/// Wraps a provider and keeps what it answers.
pub struct RecordingProvider {
    inner: Arc<dyn ModelProvider>,
    /// One slot per call, claimed when the call is made, so the cassette is
    /// in call order even when concurrent calls finish in another.
    calls: Mutex<Vec<Option<Value>>>,
}

impl RecordingProvider {
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        Self { inner, calls: Mutex::new(Vec::new()) }
    }

    /// Everything recorded so far, as a cassette document:
    /// `{"version": 1, "calls": [{agent, model, fingerprint, response}]}`.
    pub fn cassette(&self) -> Value {
        let calls = self.calls.lock().unwrap_or_else(|p| p.into_inner());
        let calls: Vec<&Value> = calls.iter().flatten().collect();
        json!({"version": CASSETTE_VERSION, "calls": calls})
    }
}

impl ModelProvider for RecordingProvider {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn complete<'a>(&'a self, req: &'a CompletionRequest<'a>) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            let slot = {
                let mut calls = self.calls.lock().unwrap_or_else(|p| p.into_inner());
                calls.push(None);
                calls.len() - 1
            };
            // A failed call leaves its slot empty: a cassette holds the
            // answers a run was built on, and a transient 529 is not one.
            let response = self.inner.complete(req).await?;
            self.calls.lock().unwrap_or_else(|p| p.into_inner())[slot] = Some(json!({
                "agent": req.agent.name,
                "model": req.agent.model,
                "fingerprint": fingerprint(req),
                "response": response_to_json(&response),
            }));
            Ok(response)
        })
    }
}

struct Recorded {
    fingerprint: String,
    response: ProviderResponse,
}

/// Answers from a cassette, with no network. See the module docs.
pub struct ReplayProvider {
    by_agent: Mutex<HashMap<String, VecDeque<Recorded>>>,
    total: usize,
    used: std::sync::atomic::AtomicUsize,
}

impl ReplayProvider {
    /// Load a cassette document. Fails (the caller reports `WC320`) when it is
    /// not one this build can read.
    pub fn from_cassette(cassette: &Value) -> Result<Self, String> {
        match cassette.get("version").and_then(Value::as_u64) {
            Some(CASSETTE_VERSION) => {}
            Some(other) => {
                return Err(format!(
                    "the cassette is format version {other}, and this build reads version {CASSETTE_VERSION}"
                ));
            }
            None => return Err("the cassette has no \"version\"".into()),
        }
        let calls = cassette
            .get("calls")
            .and_then(Value::as_array)
            .ok_or("the cassette has no \"calls\" array")?;
        let mut by_agent: HashMap<String, VecDeque<Recorded>> = HashMap::new();
        for (i, call) in calls.iter().enumerate() {
            let agent = call.get("agent").and_then(Value::as_str).ok_or_else(|| format!("call {i} names no agent"))?;
            let fingerprint = call
                .get("fingerprint")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("call {i} has no fingerprint"))?
                .to_string();
            let response = response_from_json(call.get("response").unwrap_or(&Value::Null))
                .map_err(|e| format!("call {i} ({agent}): {e}"))?;
            by_agent.entry(agent.to_string()).or_default().push_back(Recorded { fingerprint, response });
        }
        Ok(Self { by_agent: Mutex::new(by_agent), total: calls.len(), used: Default::default() })
    }

    /// `{"recorded", "used", "unused"}`: an eval reports responses the run
    /// never asked for, because a run that got shorter has changed too.
    pub fn stats(&self) -> Value {
        let used = self.used.load(std::sync::atomic::Ordering::SeqCst);
        json!({"recorded": self.total, "used": used, "unused": self.total.saturating_sub(used)})
    }
}

impl ModelProvider for ReplayProvider {
    fn name(&self) -> &'static str {
        "replay"
    }

    fn complete<'a>(&'a self, req: &'a CompletionRequest<'a>) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            let agent = &req.agent.name;
            let mut by_agent = self.by_agent.lock().unwrap_or_else(|p| p.into_inner());
            let Some(next) = by_agent.get_mut(agent).and_then(VecDeque::pop_front) else {
                return Err(format!(
                    "WC319: the cassette has no more recorded responses for {agent:?}: this run makes a \
                     model call the recorded run did not"
                ));
            };
            let now = fingerprint(req);
            if next.fingerprint != now {
                return Err(format!(
                    "WC319: the cassette is stale for {agent:?}: its system prompt, model, tools or \
                     options changed since it was recorded (fingerprint {} then, {now} now)",
                    next.fingerprint
                ));
            }
            self.used.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(next.response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::provider::FakeProvider;
    use crate::agent::{Conversation, Message, ToolSpec};
    use crate::manifest::AgentDef;

    fn agent(name: &str, system: &str) -> AgentDef {
        serde_json::from_value(json!({"name": name, "model": "m", "system": system})).unwrap()
    }

    fn convo(text: &str) -> Conversation {
        Conversation { messages: vec![Message { role: "user".into(), content: json!(text) }] }
    }

    async fn ask(p: &dyn ModelProvider, def: &AgentDef, tools: &[ToolSpec], text: &str) -> Result<ProviderResponse, String> {
        let c = convo(text);
        let req = CompletionRequest { agent: def, conversation: &c, tools, force_tool: None, system_suffix: "" };
        p.complete(&req).await
    }

    #[tokio::test]
    async fn a_recorded_run_replays_identically_without_the_provider() {
        let rec = RecordingProvider::new(Arc::new(FakeProvider));
        let a = agent("a", "be brief");
        let first = ask(&rec, &a, &[], "tool:lookup {\"q\": 1}").await.unwrap();
        let second = ask(&rec, &a, &[], "hello").await.unwrap();
        let cassette = rec.cassette();
        assert_eq!(cassette["version"], 1);
        assert_eq!(cassette["calls"].as_array().unwrap().len(), 2);

        // Through a JSON round trip, as it is when read back from a file.
        let cassette: Value = serde_json::from_str(&cassette.to_string()).unwrap();
        let replay = ReplayProvider::from_cassette(&cassette).unwrap();
        // The conversation differs from the recorded one; only the agent has to match.
        let r1 = ask(&replay, &a, &[], "anything").await.unwrap();
        assert_eq!(r1.stop_reason, StopReason::ToolUse);
        assert_eq!(r1.tool_calls[0].name, first.tool_calls[0].name);
        assert_eq!(r1.tool_calls[0].arguments, json!({"q": 1}));
        assert_eq!(r1.raw_content, first.raw_content);
        let r2 = ask(&replay, &a, &[], "anything").await.unwrap();
        assert_eq!(r2.text, second.text);
        assert_eq!(r2.input_tokens, second.input_tokens);
        assert_eq!(replay.stats(), json!({"recorded": 2, "used": 2, "unused": 0}));

        let err = ask(&replay, &a, &[], "one more").await.unwrap_err();
        assert!(err.starts_with("WC319") && err.contains("no more recorded responses"), "{err}");
    }

    #[tokio::test]
    async fn a_changed_agent_makes_the_cassette_stale() {
        let rec = RecordingProvider::new(Arc::new(FakeProvider));
        ask(&rec, &agent("a", "be brief"), &[], "hi").await.unwrap();
        let cassette = rec.cassette();

        let replay = ReplayProvider::from_cassette(&cassette).unwrap();
        let err = ask(&replay, &agent("a", "be thorough"), &[], "hi").await.unwrap_err();
        assert!(err.starts_with("WC319") && err.contains("stale"), "{err}");

        let replay = ReplayProvider::from_cassette(&cassette).unwrap();
        let tool = ToolSpec { name: "t".into(), description: "new".into(), input_schema: json!({"type": "object"}) };
        let err = ask(&replay, &agent("a", "be brief"), &[tool], "hi").await.unwrap_err();
        assert!(err.contains("stale"), "{err}");

        let replay = ReplayProvider::from_cassette(&cassette).unwrap();
        let err = ask(&replay, &agent("b", "be brief"), &[], "hi").await.unwrap_err();
        assert!(err.contains("no more recorded responses for \"b\""), "{err}");
        assert_eq!(replay.stats()["unused"], 1);
    }

    #[tokio::test]
    async fn failed_calls_are_not_recorded() {
        let rec = RecordingProvider::new(Arc::new(FakeProvider));
        assert!(ask(&rec, &agent("a", ""), &[], "fail:anthropic 529: overloaded").await.is_err());
        assert_eq!(rec.cassette()["calls"], json!([]));
    }

    #[test]
    fn an_unreadable_cassette_says_why() {
        for (bad, why) in [
            (json!({}), "no \"version\""),
            (json!({"version": 9, "calls": []}), "version 9"),
            (json!({"version": 1}), "no \"calls\""),
            (json!({"version": 1, "calls": [{"agent": "a"}]}), "no fingerprint"),
            (json!({"version": 1, "calls": [{"agent": "a", "fingerprint": "x", "response": {"stop_reason": "end_turn"}}]}), "no model"),
            (json!({"version": 1, "calls": [{"agent": "a", "fingerprint": "x", "response": {"stop_reason": "??"}}]}), "stop_reason"),
        ] {
            let err = ReplayProvider::from_cassette(&bad).err().unwrap();
            assert!(err.contains(why), "{err}");
        }
    }

    #[test]
    fn the_fingerprint_follows_the_agent_not_the_conversation() {
        let a = agent("a", "s");
        let (c1, c2) = (convo("one"), convo("two"));
        let f = |def: &AgentDef, c: &Conversation, force: Option<&str>| {
            fingerprint(&CompletionRequest { agent: def, conversation: c, tools: &[], force_tool: force, system_suffix: "x" })
        };
        assert_eq!(f(&a, &c1, None), f(&a, &c2, None));
        assert_ne!(f(&a, &c1, None), f(&a, &c1, Some("respond")));
        let mut with_options = a.clone();
        with_options.options.insert("seed".into(), json!(7));
        assert_ne!(f(&a, &c1, None), f(&with_options, &c1, None));
        assert_eq!(f(&a, &c1, None).len(), 16);
    }
}
