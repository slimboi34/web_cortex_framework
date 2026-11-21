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
//! stable parts — the agent's system prompt, its model name and the concrete
//! model that name resolves to through the app's aliases, its tools and their
//! schemas, a forced tool, its request options merged over its provider's,
//! and its context providers (their names, in order, and the static parts of
//! each definition: description, size limit, source kind and the constant or
//! SQL it reads). If the fingerprint of the call being replayed differs, the
//! cassette is stale: the agent was changed since it was recorded, the
//! recorded answer is an answer to a different question, and the call fails
//! with `WC319` rather than pretend. The conversation itself is not
//! fingerprinted, because tool results carry run ids and timings that differ
//! on every run; for the same reason a context provider's rendered value is
//! not, only its definition.
//!
//! What the fingerprint needs from the app beyond the request (aliases,
//! provider options, context definitions) is a [`FingerprintScope`], given to
//! both providers with `with_scope`/`set_scope`. Cassettes recorded by 2.8,
//! whose fingerprint covered less, are reported stale once and say so.

use super::provider::{CompletionRequest, ModelProvider, ProviderResponse, StopReason, ToolCall};
use super::registry::{aliases_of, merge_options, resolve_alias_in};
use crate::manifest::{ContextDef, ContextSource, ModelsConfig, RequestOptions};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// The cassette format this build reads and writes.
pub const CASSETTE_VERSION: u64 = 1;

/// What the fingerprint covers, stored on each recorded call as
/// `fingerprint_version`. 2.8 recorded no version (read as 1): its
/// fingerprint left out alias resolution, provider options and context
/// providers, so its cassettes go stale once, and the message says why.
pub const FINGERPRINT_VERSION: u64 = 2;

/// What a fingerprint needs from the application beyond the request itself:
/// where the agent's model alias points, the request options set for each
/// provider (`app.request_options`), and the definition of each context
/// provider. Built once per app with [`FingerprintScope::of`].
#[derive(Debug, Clone)]
pub struct FingerprintScope {
    aliases: BTreeMap<String, String>,
    provider_options: BTreeMap<String, RequestOptions>,
    contexts: BTreeMap<String, Value>,
}

impl Default for FingerprintScope {
    /// Only the built-in aliases; no provider options, no context definitions.
    fn default() -> Self {
        Self::new(&ModelsConfig::default(), &[])
    }
}

impl FingerprintScope {
    pub fn new(models: &ModelsConfig, contexts: &[ContextDef]) -> Self {
        Self {
            aliases: aliases_of(models),
            provider_options: models.options.clone(),
            contexts: contexts.iter().map(|c| (c.name.clone(), context_definition(c))).collect(),
        }
    }

    /// The scope of a built app: its manifest's models and context providers.
    pub fn of(app: &crate::app::App) -> Self {
        Self::new(&app.manifest.models, &app.manifest.contexts)
    }
}

/// The parts of a context provider that are fixed by its declaration. A
/// query's rows and a Python handler's result change from run to run and are
/// left out, and so is the handler's number, which moves when routes are added.
fn context_definition(c: &ContextDef) -> Value {
    let source = match &c.source {
        ContextSource::Static { value } => json!({"kind": "static", "value": value}),
        ContextSource::Query { sql, params, returns } => {
            json!({"kind": "query", "sql": sql, "params": params, "returns": returns})
        }
        ContextSource::Python { .. } => json!({"kind": "python"}),
    };
    json!({"description": c.description, "max_chars": c.max_chars, "source": source})
}

/// The stable parts of a completion request, hashed, with only the built-in
/// aliases known. See [`fingerprint_in`].
pub fn fingerprint(req: &CompletionRequest<'_>) -> String {
    fingerprint_in(req, &FingerprintScope::default())
}

/// The stable parts of a completion request, hashed. See the module docs for
/// what is and is not included, and why.
pub fn fingerprint_in(req: &CompletionRequest<'_>, scope: &FingerprintScope) -> String {
    let a = req.agent;
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!([t.name, t.description, t.input_schema]))
        .collect();
    let resolved = resolve_alias_in(&scope.aliases, &a.model);
    let options = merge_options(&scope.provider_options, &resolved, &a.options);
    let contexts: Vec<Value> = a.context.iter().map(|name| json!([name, scope.contexts.get(name)])).collect();
    let canonical = json!([
        a.name, a.model, resolved, a.system, tools, req.force_tool, options, a.max_tokens,
        a.temperature.map(|t| t.to_string()), a.reasoning, contexts,
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
    scope: FingerprintScope,
}

impl RecordingProvider {
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        Self { inner, calls: Mutex::new(Vec::new()), scope: FingerprintScope::default() }
    }

    /// Fingerprint against this app's aliases, provider options and context
    /// providers. Give the replaying side the same scope.
    pub fn with_scope(mut self, scope: FingerprintScope) -> Self {
        self.scope = scope;
        self
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
                "fingerprint": fingerprint_in(req, &self.scope),
                "fingerprint_version": FINGERPRINT_VERSION,
                "response": response_to_json(&response),
            }));
            Ok(response)
        })
    }
}

struct Recorded {
    fingerprint: String,
    /// [`FINGERPRINT_VERSION`] when it was recorded; 1 for a 2.8 cassette.
    version: u64,
    response: ProviderResponse,
}

/// Answers from a cassette, with no network. See the module docs.
pub struct ReplayProvider {
    by_agent: Mutex<HashMap<String, VecDeque<Recorded>>>,
    total: usize,
    used: std::sync::atomic::AtomicUsize,
    scope: OnceLock<FingerprintScope>,
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
            let version = call.get("fingerprint_version").and_then(Value::as_u64).unwrap_or(1);
            let response = response_from_json(call.get("response").unwrap_or(&Value::Null))
                .map_err(|e| format!("call {i} ({agent}): {e}"))?;
            by_agent.entry(agent.to_string()).or_default().push_back(Recorded { fingerprint, version, response });
        }
        Ok(Self { by_agent: Mutex::new(by_agent), total: calls.len(), used: Default::default(), scope: OnceLock::new() })
    }

    /// Fingerprint against this app's aliases, provider options and context
    /// providers: the scope the cassette was recorded with.
    pub fn with_scope(self, scope: FingerprintScope) -> Self {
        let _ = self.scope.set(scope);
        self
    }

    /// [`Self::with_scope`] for a provider already shared, before its first
    /// call. A scope can be set once; false when one already was.
    pub fn set_scope(&self, scope: FingerprintScope) -> bool {
        self.scope.set(scope).is_ok()
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
            let now = match self.scope.get() {
                Some(scope) => fingerprint_in(req, scope),
                None => fingerprint(req),
            };
            if next.fingerprint != now {
                let older = if next.version < FINGERPRINT_VERSION {
                    "; it was recorded by WebCortex 2.8 or earlier, whose fingerprint covered less, so it \
                     has to be recorded again once"
                } else {
                    ""
                };
                return Err(format!(
                    "WC319: the cassette is stale for {agent:?}: its system prompt, model (or what its \
                     alias points to), tools, context providers or request options (its own or its \
                     provider's) changed since it was recorded (fingerprint {} then, {now} now){older}",
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

    fn models(aliases: &[(&str, &str)], options: &[(&str, Value)]) -> ModelsConfig {
        let mut m = ModelsConfig::default();
        for (k, v) in aliases {
            m.aliases.insert(k.to_string(), v.to_string());
        }
        for (provider, opts) in options {
            m.options.insert(provider.to_string(), opts.as_object().unwrap().clone());
        }
        m
    }

    fn context(name: &str, source: Value) -> ContextDef {
        serde_json::from_value(json!({"name": name, "source": source})).unwrap()
    }

    #[test]
    fn the_fingerprint_covers_aliases_provider_options_and_context_providers() {
        let c = convo("hi");
        let fp = |def: &AgentDef, scope: &FingerprintScope| {
            fingerprint_in(&CompletionRequest { agent: def, conversation: &c, tools: &[], force_tool: None, system_suffix: "" }, scope)
        };
        let mut desk = agent("desk", "Answer billing questions.");
        desk.model = "fast".into();
        let base = FingerprintScope::new(&models(&[("fast", "claude-haiku-4-5")], &[]), &[]);
        let f0 = fp(&desk, &base);
        assert_eq!(f0, fp(&desk, &base.clone()), "deterministic");

        // The alias now names another model.
        assert_ne!(f0, fp(&desk, &FingerprintScope::new(&models(&[("fast", "claude-opus-5")], &[]), &[])));
        // Options set for the provider the alias resolves to; another provider's do not count.
        let opts = |p: &str| FingerprintScope::new(&models(&[("fast", "claude-haiku-4-5")], &[(p, json!({"top_k": 5}))]), &[]);
        assert_ne!(f0, fp(&desk, &opts("anthropic")));
        assert_eq!(f0, fp(&desk, &opts("openai")));
        // The agent's own option overrides the provider's to the same merged value.
        let mut own = desk.clone();
        own.options.insert("top_k".into(), json!(5));
        assert_eq!(fp(&own, &base), fp(&desk, &opts("anthropic")));

        // A context provider added to the agent, and a change to its definition.
        let policy = |rule: &str| context("policy", json!({"kind": "static", "value": {"rule": rule}}));
        let scope = |defs: &[ContextDef]| FingerprintScope::new(&models(&[("fast", "claude-haiku-4-5")], &[]), defs);
        let mut with_ctx = desk.clone();
        with_ctx.context = vec!["policy".into()];
        let f1 = fp(&with_ctx, &scope(&[policy("no refunds over 50")]));
        assert_ne!(f0, f1);
        assert_ne!(f1, fp(&with_ctx, &scope(&[policy("no refunds over 80")])));
        assert_eq!(fp(&desk, &scope(&[policy("no refunds over 80")])), f0, "a provider the agent does not use is not part of it");
        // Even with no scope, adding a context provider changes the fingerprint.
        assert_ne!(fingerprint(&CompletionRequest { agent: &desk, conversation: &c, tools: &[], force_tool: None, system_suffix: "" }),
                   fingerprint(&CompletionRequest { agent: &with_ctx, conversation: &c, tools: &[], force_tool: None, system_suffix: "" }));
        // A query provider's SQL counts; a Python provider's handler number does not.
        let q = |sql: &str| context("policy", json!({"kind": "query", "sql": sql, "params": ["input"]}));
        assert_ne!(fp(&with_ctx, &scope(&[q("SELECT 1")])), fp(&with_ctx, &scope(&[q("SELECT 2")])));
        let py = |h: u32| context("policy", json!({"kind": "python", "handler": h}));
        assert_eq!(fp(&with_ctx, &scope(&[py(1)])), fp(&with_ctx, &scope(&[py(7)])));
        // The rendered suffix (live data) is not fingerprinted.
        let r = |suffix: &str| fingerprint_in(&CompletionRequest { agent: &with_ctx, conversation: &c, tools: &[], force_tool: None, system_suffix: suffix }, &scope(&[py(1)]));
        assert_eq!(r("<context>rows A</context>"), r("<context>rows B</context>"));
    }

    #[tokio::test]
    async fn a_scoped_cassette_replays_and_goes_stale_when_the_alias_moves() {
        let mut desk = agent("desk", "s");
        desk.model = "fast".into();
        let scope = |target: &str| FingerprintScope::new(&models(&[("fast", target)], &[]), &[]);
        let rec = RecordingProvider::new(Arc::new(FakeProvider)).with_scope(scope("claude-haiku-4-5"));
        ask(&rec, &desk, &[], "hi").await.unwrap();
        let cassette = rec.cassette();
        assert_eq!(cassette["calls"][0]["fingerprint_version"], FINGERPRINT_VERSION);

        let same = ReplayProvider::from_cassette(&cassette).unwrap().with_scope(scope("claude-haiku-4-5"));
        assert!(ask(&same, &desk, &[], "hi").await.is_ok());
        let moved = ReplayProvider::from_cassette(&cassette).unwrap();
        assert!(moved.set_scope(scope("claude-opus-5")));
        assert!(!moved.set_scope(scope("claude-haiku-4-5")), "set once");
        let err = ask(&moved, &desk, &[], "hi").await.unwrap_err();
        assert!(err.starts_with("WC319: the cassette is stale") && err.contains("alias points to"), "{err}");
        assert!(!err.contains("2.8"), "{err}");

        // A cassette from 2.8 (no fingerprint_version) says why it is stale.
        let mut old = cassette.clone();
        old["calls"][0].as_object_mut().unwrap().remove("fingerprint_version");
        old["calls"][0]["fingerprint"] = json!("5d98eea00be35bdd");
        let err = ask(&ReplayProvider::from_cassette(&old).unwrap(), &desk, &[], "hi").await.unwrap_err();
        assert!(err.contains("recorded by WebCortex 2.8 or earlier"), "{err}");
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
