//! Run traces: what a request tree did, as spans.
//!
//! The step list in a run result says what one agent did. It does not say what
//! the worker it called did, how long the flow around it took, or where the
//! tokens of the whole tree went. A trace does: every agent run, model call,
//! tool call, handoff, flow and behaviour in one request tree is a span with a
//! parent, a duration and, for model calls, token counts.
//!
//! * **In memory**, the last [`TracingConfig::keep`] traces are served by
//!   `GET /_webcortex/traces` and `/traces/{id}` (and `webcortex trace`).
//! * **Out of the process**, finished spans are exported as OTLP/HTTP JSON
//!   when an endpoint is configured, with the OpenTelemetry GenAI attribute
//!   names (`gen_ai.request.model`, `gen_ai.usage.input_tokens`, …), so the
//!   collector the operator already runs can read them.
//! * **Across services**, an incoming W3C `traceparent` header is honoured:
//!   the run joins the caller's trace instead of starting its own.
//!
//! Only routes that run a model open spans; a plain route never touches the
//! tracer. Like the status recorder, a span never holds a prompt, a tool
//! argument, a result or a credential: names, timings, counts and codes only.
//!
//! The context travels explicitly ([`TraceCtx`] on
//! [`crate::http::WebCortexRequest`] and [`crate::agent::RunOptions`]) rather
//! than in a task-local, because a behaviour re-enters the runtime from a
//! Python worker thread, where no task-local would follow it.

use crate::manifest::TracingConfig;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// A span's address: what a child needs to attach to it. Cheap to clone.
#[derive(Debug, Clone)]
pub struct TraceCtx {
    /// 32 lowercase hex digits, as in a W3C `traceparent`.
    pub trace_id: Arc<str>,
    /// The span this context names; a child's parent.
    pub span_id: u64,
    /// False when the trace was already at `max_spans`: the context still
    /// carries the trace id to children, but ending it changes nothing.
    recorded: bool,
}

impl TraceCtx {
    /// The `traceparent` header value naming this span, for an outgoing call.
    pub fn traceparent(&self) -> String {
        format!("00-{}-{:016x}-01", self.trace_id, self.span_id)
    }
}

/// How a span ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStatus {
    Open,
    Ok,
    Error,
    /// Dropped before it finished: a request timeout or a client that left.
    Cancelled,
}

impl SpanStatus {
    fn as_str(self) -> &'static str {
        match self {
            SpanStatus::Open => "open",
            SpanStatus::Ok => "ok",
            SpanStatus::Error => "error",
            SpanStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
struct SpanRecord {
    span_id: u64,
    parent: Option<u64>,
    /// The caller's span when the trace was joined from a `traceparent`.
    remote_parent: Option<String>,
    kind: &'static str,
    name: String,
    start_ns: u64,
    end_ns: Option<u64>,
    status: SpanStatus,
    code: Option<String>,
    attrs: Map<String, Value>,
}

#[derive(Debug)]
struct TraceRecord {
    trace_id: Arc<str>,
    spans: Vec<SpanRecord>,
    dropped: u32,
}

/// One finished span on its way to the exporter.
struct ExportSpan {
    trace_id: Arc<str>,
    span: SpanRecord,
}

/// The trace store and, when configured, the door to the exporter.
pub struct Tracer {
    cfg: TracingConfig,
    traces: Mutex<VecDeque<TraceRecord>>,
    export: OnceLock<tokio::sync::mpsc::Sender<ExportSpan>>,
    exported: AtomicU64,
    export_failed: AtomicU64,
    export_dropped: AtomicU64,
    last_export_error: Mutex<Option<String>>,
}

/// Where spans are exported to, resolved from the manifest and the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportTarget {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn new_span_id() -> u64 {
    loop {
        let id = uuid::Uuid::new_v4().as_u128() as u64;
        if id != 0 {
            return id;
        }
    }
}

/// The trace id and parent span id of a W3C `traceparent` header, when it is
/// well formed: `00-<32 hex>-<16 hex>-<2 hex>`, neither id all zeros.
pub fn parse_traceparent(header: &str) -> Option<(String, String)> {
    let mut parts = header.trim().split('-');
    let (version, trace, span, flags) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hex(version, 2) || version == "ff" || !hex(trace, 32) || !hex(span, 16) || !hex(flags, 2) {
        return None;
    }
    if trace.bytes().all(|b| b == b'0') || span.bytes().all(|b| b == b'0') {
        return None;
    }
    Some((trace.to_string(), span.to_string()))
}

impl Tracer {
    pub fn new(cfg: &TracingConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            traces: Mutex::new(VecDeque::new()),
            export: OnceLock::new(),
            exported: AtomicU64::new(0),
            export_failed: AtomicU64::new(0),
            export_dropped: AtomicU64::new(0),
            last_export_error: Mutex::new(None),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Open a span. With a `parent` it joins that trace; without one it starts
    /// a trace, adopting the id in `traceparent` when the caller sent a valid
    /// one. Returns `None` only when tracing is off.
    pub fn begin(
        &self,
        parent: Option<&TraceCtx>,
        traceparent: Option<&str>,
        kind: &'static str,
        name: &str,
    ) -> Option<TraceCtx> {
        if !self.cfg.enabled {
            return None;
        }
        let remote = if parent.is_none() { traceparent.and_then(parse_traceparent) } else { None };
        let trace_id: Arc<str> = match (parent, &remote) {
            (Some(p), _) => p.trace_id.clone(),
            (None, Some((trace, _))) => Arc::from(trace.as_str()),
            (None, None) => Arc::from(uuid::Uuid::new_v4().simple().to_string().as_str()),
        };
        let span_id = new_span_id();
        let mut traces = self.traces.lock().unwrap_or_else(|p| p.into_inner());
        let at = match traces.iter().rposition(|t| t.trace_id == trace_id) {
            Some(i) => i,
            None => {
                while traces.len() >= self.cfg.keep.max(1) {
                    traces.pop_front();
                }
                traces.push_back(TraceRecord { trace_id: trace_id.clone(), spans: Vec::new(), dropped: 0 });
                traces.len() - 1
            }
        };
        let record = &mut traces[at];
        if record.spans.len() >= self.cfg.max_spans.max(1) {
            record.dropped = record.dropped.saturating_add(1);
            // Children still learn which trace they belong to (and are
            // dropped in turn); they hang off the nearest recorded ancestor.
            return Some(TraceCtx { trace_id, span_id: parent.map(|p| p.span_id).unwrap_or(span_id), recorded: false });
        }
        record.spans.push(SpanRecord {
            span_id,
            parent: parent.map(|p| p.span_id),
            remote_parent: remote.map(|(_, span)| span),
            kind,
            name: name.to_string(),
            start_ns: now_ns(),
            end_ns: None,
            status: SpanStatus::Open,
            code: None,
            attrs: Map::new(),
        });
        Some(TraceCtx { trace_id, span_id, recorded: true })
    }

    /// Open a span that ends when the guard is dropped, as cancelled unless
    /// [`Scoped::ok`] or [`Scoped::fail`] was called first. For spans that live
    /// inside one function: a model call, a tool call.
    pub fn scoped<'a>(&'a self, parent: Option<&TraceCtx>, kind: &'static str, name: &str) -> Scoped<'a> {
        // A child with no parent would start a trace of its own; a scoped
        // span only ever exists inside one.
        let ctx = parent.and_then(|p| self.begin(Some(p), None, kind, name));
        Scoped { tracer: self, ctx, attrs: Vec::new() }
    }

    /// Record a span that took no time: a refusal, a handoff.
    pub fn event(&self, parent: Option<&TraceCtx>, kind: &'static str, name: &str, code: Option<&str>, attrs: Vec<(&str, Value)>) {
        if let Some(ctx) = parent.and_then(|p| self.begin(Some(p), None, kind, name)) {
            let status = if code.is_some() { SpanStatus::Error } else { SpanStatus::Ok };
            self.end(&ctx, status, code, attrs);
        }
    }

    /// Close a span. Ending one twice, or one whose trace has been evicted,
    /// is not an error.
    pub fn end(&self, ctx: &TraceCtx, status: SpanStatus, code: Option<&str>, attrs: Vec<(&str, Value)>) {
        if !ctx.recorded {
            return;
        }
        let finished = {
            let mut traces = self.traces.lock().unwrap_or_else(|p| p.into_inner());
            let Some(record) = traces.iter_mut().rev().find(|t| t.trace_id == ctx.trace_id) else { return };
            let Some(span) = record.spans.iter_mut().rev().find(|s| s.span_id == ctx.span_id) else { return };
            if span.end_ns.is_some() {
                return;
            }
            // Never before the start, whatever the wall clock did meanwhile.
            span.end_ns = Some(now_ns().max(span.start_ns));
            span.status = status;
            span.code = code.map(str::to_string);
            for (k, v) in attrs {
                span.attrs.insert(k.to_string(), v);
            }
            self.export.get().map(|_| span.clone())
        };
        if let (Some(span), Some(tx)) = (finished, self.export.get())
            && tx.try_send(ExportSpan { trace_id: ctx.trace_id.clone(), span }).is_err()
        {
            self.export_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The recent traces, newest first, one summary line each.
    pub fn list(&self) -> Value {
        let traces = self.traces.lock().unwrap_or_else(|p| p.into_inner());
        let items: Vec<Value> = traces.iter().rev().map(summarise).collect();
        json!({
            "enabled": self.cfg.enabled,
            "kept": items.len(),
            "capacity": self.cfg.keep,
            "traces": items,
            "export": self.export_state(),
        })
    }

    /// One trace with every span, in start order, or `None` when it is not
    /// (or no longer) held.
    pub fn get(&self, trace_id: &str) -> Option<Value> {
        let traces = self.traces.lock().unwrap_or_else(|p| p.into_inner());
        let record = traces.iter().rev().find(|t| &*t.trace_id == trace_id)?;
        let origin = record.spans.iter().map(|s| s.start_ns).min().unwrap_or(0);
        let mut spans: Vec<&SpanRecord> = record.spans.iter().collect();
        spans.sort_by_key(|s| s.start_ns);
        let mut out = summarise(record);
        out["spans"] = Value::Array(
            spans
                .iter()
                .map(|s| {
                    let mut v = json!({
                        "span_id": format!("{:016x}", s.span_id),
                        "parent_span_id": s.parent.map(|p| format!("{p:016x}")).or_else(|| s.remote_parent.clone()),
                        "kind": s.kind,
                        "name": s.name,
                        "start_ms": millis(s.start_ns.saturating_sub(origin)),
                        "duration_ms": s.end_ns.map(|e| millis(e.saturating_sub(s.start_ns))),
                        "status": s.status.as_str(),
                        "attributes": s.attrs,
                    });
                    if let Some(code) = &s.code {
                        v["code"] = json!(code);
                    }
                    if s.remote_parent.is_some() {
                        v["remote_parent"] = json!(true);
                    }
                    v
                })
                .collect(),
        );
        Some(out)
    }

    /// The newest trace's id, if any is held.
    pub fn latest(&self) -> Option<String> {
        let traces = self.traces.lock().unwrap_or_else(|p| p.into_inner());
        traces.back().map(|t| t.trace_id.to_string())
    }

    fn export_state(&self) -> Value {
        json!({
            "configured": self.export.get().is_some(),
            "spans_exported": self.exported.load(Ordering::Relaxed),
            "spans_failed": self.export_failed.load(Ordering::Relaxed),
            "spans_dropped": self.export_dropped.load(Ordering::Relaxed),
            "last_error": *self.last_export_error.lock().unwrap_or_else(|p| p.into_inner()),
        })
    }

    /// Where this app's spans should be exported, if anywhere: the manifest's
    /// endpoint, else `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (used as given),
    /// else `OTEL_EXPORTER_OTLP_ENDPOINT` (with `/v1/traces` appended).
    /// Headers come from the variable the manifest names, else
    /// `OTEL_EXPORTER_OTLP_HEADERS`, in its `k=v,k2=v2` form.
    pub fn export_target(&self, env: impl Fn(&str) -> Option<String>) -> Option<ExportTarget> {
        if !self.cfg.enabled {
            return None;
        }
        let with_path = |base: &str| {
            let base = base.trim_end_matches('/');
            if base.ends_with("/v1/traces") { base.to_string() } else { format!("{base}/v1/traces") }
        };
        let nonempty = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let url = match nonempty(self.cfg.otlp_endpoint.clone()) {
            Some(declared) => with_path(&declared),
            None => match nonempty(env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")) {
                Some(exact) => exact,
                None => with_path(&nonempty(env("OTEL_EXPORTER_OTLP_ENDPOINT"))?),
            },
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return None;
        }
        let var = self.cfg.otlp_headers_env.as_deref().unwrap_or("OTEL_EXPORTER_OTLP_HEADERS");
        let headers = env(var)
            .unwrap_or_default()
            .split(',')
            .filter_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                let (k, v) = (k.trim(), v.trim());
                (!k.is_empty()).then(|| (k.to_string(), v.to_string()))
            })
            .collect();
        Some(ExportTarget { url, headers })
    }

    /// Start exporting finished spans to `target` in batches. Called once, by
    /// the server; the task ends when `shutdown` flips, after one last flush.
    pub fn start_export(
        self: &Arc<Self>,
        client: reqwest::Client,
        target: ExportTarget,
        service: (String, String),
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ExportSpan>(EXPORT_QUEUE);
        if self.export.set(tx).is_err() {
            return None;
        }
        let tracer = self.clone();
        Some(tokio::spawn(async move {
            let mut batch: Vec<ExportSpan> = Vec::new();
            loop {
                let stopping = tokio::select! {
                    span = rx.recv() => match span {
                        Some(span) => { batch.push(span); false }
                        None => true,
                    },
                    _ = tokio::time::sleep(EXPORT_INTERVAL), if !batch.is_empty() => false,
                    _ = shutdown.changed() => true,
                };
                while batch.len() < EXPORT_BATCH
                    && let Ok(span) = rx.try_recv()
                {
                    batch.push(span);
                }
                let due = stopping || batch.len() >= EXPORT_BATCH || batch.first().is_some_and(|s| {
                    now_ns().saturating_sub(s.span.end_ns.unwrap_or(0)) >= EXPORT_INTERVAL.as_nanos() as u64
                });
                if due && !batch.is_empty() {
                    let n = batch.len() as u64;
                    let body = otlp_body(&batch, &service.0, &service.1);
                    batch.clear();
                    let mut req = client.post(&target.url).timeout(EXPORT_TIMEOUT).json(&body);
                    for (k, v) in &target.headers {
                        req = req.header(k, v);
                    }
                    let failure = match req.send().await {
                        Ok(res) if res.status().is_success() => None,
                        Ok(res) => Some(format!("{} answered {}", target.url, res.status())),
                        Err(e) => Some(format!("{} could not be reached: {e}", target.url)),
                    };
                    match failure {
                        None => {
                            tracer.exported.fetch_add(n, Ordering::Relaxed);
                        }
                        Some(why) => {
                            // Dropped, not retried: a collector that is down
                            // must not grow a queue inside the app it observes.
                            tracer.export_failed.fetch_add(n, Ordering::Relaxed);
                            tracing::warn!(error = %why, spans = n, "trace export failed; the spans were dropped");
                            *tracer.last_export_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(why);
                        }
                    }
                }
                if stopping {
                    break;
                }
            }
        }))
    }
}

const EXPORT_QUEUE: usize = 4096;
const EXPORT_BATCH: usize = 256;
const EXPORT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);
const EXPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A span that closes itself. See [`Tracer::scoped`].
pub struct Scoped<'a> {
    tracer: &'a Tracer,
    ctx: Option<TraceCtx>,
    attrs: Vec<(&'static str, Value)>,
}

impl Scoped<'_> {
    /// The context children of this span are opened with.
    pub fn ctx(&self) -> Option<&TraceCtx> {
        self.ctx.as_ref()
    }

    /// Attach an attribute, written when the span ends.
    pub fn set(&mut self, key: &'static str, value: Value) {
        if self.ctx.is_some() {
            self.attrs.push((key, value));
        }
    }

    pub fn ok(mut self) {
        self.finish(SpanStatus::Ok, None);
    }

    pub fn fail(mut self, code: &str) {
        self.finish(SpanStatus::Error, Some(code));
    }

    fn finish(&mut self, status: SpanStatus, code: Option<&str>) {
        if let Some(ctx) = self.ctx.take() {
            let attrs = std::mem::take(&mut self.attrs);
            self.tracer.end(&ctx, status, code, attrs.into_iter().collect());
        }
    }
}

impl Drop for Scoped<'_> {
    fn drop(&mut self) {
        self.finish(SpanStatus::Cancelled, None);
    }
}

fn millis(ns: u64) -> f64 {
    (ns as f64 / 1_000.0).round() / 1_000.0
}

fn summarise(record: &TraceRecord) -> Value {
    let root = record.spans.iter().find(|s| s.parent.is_none()).or(record.spans.first());
    let start = record.spans.iter().map(|s| s.start_ns).min().unwrap_or(0);
    let open = record.spans.iter().filter(|s| s.end_ns.is_none()).count();
    let end = record.spans.iter().filter_map(|s| s.end_ns).max();
    let count = |kind: &str| record.spans.iter().filter(|s| s.kind == kind).count();
    let sum = |key: &str| -> u64 {
        record.spans.iter().filter_map(|s| s.attrs.get(key).and_then(Value::as_u64)).fold(0, u64::saturating_add)
    };
    let errors: Vec<&str> = record.spans.iter().filter_map(|s| s.code.as_deref()).collect();
    json!({
        "trace_id": &*record.trace_id,
        "name": root.map(|s| s.name.as_str()).unwrap_or(""),
        "kind": root.map(|s| s.kind).unwrap_or(""),
        "status": root.map(|s| s.status.as_str()).unwrap_or("open"),
        "started_unix_ms": start / 1_000_000,
        "duration_ms": if open == 0 { end.map(|e| millis(e.saturating_sub(start))) } else { None },
        "spans": record.spans.len(),
        "open_spans": open,
        "dropped_spans": record.dropped,
        "model_calls": count("model"),
        "tool_calls": count("tool"),
        "input_tokens": sum("gen_ai.usage.input_tokens"),
        "output_tokens": sum("gen_ai.usage.output_tokens"),
        "cache_tokens": sum("webcortex.usage.cache_read_tokens").saturating_add(sum("webcortex.usage.cache_write_tokens")),
        "errors": errors,
    })
}

fn otlp_value(v: &Value) -> Value {
    match v {
        Value::String(s) => json!({"stringValue": s}),
        Value::Bool(b) => json!({"boolValue": b}),
        // OTLP/JSON spells 64-bit integers as strings.
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({"intValue": n.to_string()}),
        Value::Number(n) => json!({"doubleValue": n.as_f64().unwrap_or(0.0)}),
        other => json!({"stringValue": other.to_string()}),
    }
}

/// An OTLP/HTTP JSON `ExportTraceServiceRequest` for a batch of spans.
fn otlp_body(batch: &[ExportSpan], service: &str, version: &str) -> Value {
    let spans: Vec<Value> = batch
        .iter()
        .map(|e| {
            let s = &e.span;
            let mut attributes: Vec<Value> = vec![json!({"key": "webcortex.span.kind", "value": {"stringValue": s.kind}})];
            attributes.extend(s.attrs.iter().map(|(k, v)| json!({"key": k, "value": otlp_value(v)})));
            if let Some(code) = &s.code {
                attributes.push(json!({"key": "webcortex.error.code", "value": {"stringValue": code}}));
            }
            let mut span = json!({
                "traceId": &*e.trace_id,
                "spanId": format!("{:016x}", s.span_id),
                "name": s.name,
                // CLIENT for the call out to a model provider, INTERNAL otherwise.
                "kind": if s.kind == "model" { 3 } else { 1 },
                "startTimeUnixNano": s.start_ns.to_string(),
                "endTimeUnixNano": s.end_ns.unwrap_or(s.start_ns).to_string(),
                "attributes": attributes,
                "status": match s.status {
                    SpanStatus::Ok => json!({"code": 1}),
                    SpanStatus::Error => json!({"code": 2, "message": s.code.clone().unwrap_or_default()}),
                    SpanStatus::Cancelled => json!({"code": 2, "message": "cancelled"}),
                    SpanStatus::Open => json!({"code": 0}),
                },
            });
            if let Some(parent) = s.parent.map(|p| format!("{p:016x}")).or_else(|| s.remote_parent.clone()) {
                span["parentSpanId"] = json!(parent);
            }
            span
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": service}},
                {"key": "service.version", "value": {"stringValue": version}},
            ]},
            "scopeSpans": [{
                "scope": {"name": "webcortex", "version": env!("CARGO_PKG_VERSION")},
                "spans": spans,
            }],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracer(keep: usize, max_spans: usize) -> Tracer {
        Tracer::new(&TracingConfig { keep, max_spans, ..Default::default() })
    }

    #[test]
    fn spans_nest_under_their_parent_and_report_tokens() {
        let t = tracer(10, 100);
        let root = t.begin(None, None, "agent", "agent desk").unwrap();
        let mut model = t.scoped(Some(&root), "model", "model m");
        model.set("gen_ai.usage.input_tokens", json!(120));
        model.set("gen_ai.usage.output_tokens", json!(30));
        model.ok();
        let tool = t.scoped(Some(&root), "tool", "tool lookup");
        let nested = t.begin(tool.ctx(), None, "agent", "agent worker").unwrap();
        t.end(&nested, SpanStatus::Ok, None, vec![]);
        tool.fail("WC308");
        t.end(&root, SpanStatus::Ok, None, vec![("webcortex.run.status", json!("completed"))]);

        let got = t.get(&root.trace_id).unwrap();
        let spans = got["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 4);
        assert_eq!(got["name"], "agent desk");
        assert_eq!(got["status"], "ok");
        assert_eq!(got["input_tokens"], 120);
        assert_eq!(got["model_calls"], 1);
        assert_eq!(got["tool_calls"], 1);
        assert_eq!(got["errors"], json!(["WC308"]));
        assert!(got["duration_ms"].is_number());
        let by_name = |n: &str| spans.iter().find(|s| s["name"] == n).unwrap();
        assert_eq!(by_name("agent desk")["parent_span_id"], Value::Null);
        assert_eq!(by_name("model m")["parent_span_id"], by_name("agent desk")["span_id"]);
        assert_eq!(by_name("agent worker")["parent_span_id"], by_name("tool lookup")["span_id"]);
        assert_eq!(by_name("tool lookup")["code"], "WC308");
        assert_eq!(by_name("agent desk")["attributes"]["webcortex.run.status"], "completed");
    }

    #[test]
    fn a_dropped_guard_is_a_cancelled_span_and_an_open_trace_has_no_duration() {
        let t = tracer(10, 100);
        let root = t.begin(None, None, "agent", "a").unwrap();
        drop(t.scoped(Some(&root), "model", "m"));
        let got = t.get(&root.trace_id).unwrap();
        assert_eq!(got["spans"][1]["status"], "cancelled");
        assert_eq!(got["open_spans"], 1);
        assert_eq!(got["duration_ms"], Value::Null);
        // Ending twice keeps the first outcome.
        t.end(&root, SpanStatus::Error, Some("WC304"), vec![]);
        t.end(&root, SpanStatus::Ok, None, vec![]);
        assert_eq!(t.get(&root.trace_id).unwrap()["status"], "error");
    }

    #[test]
    fn a_scoped_span_without_a_parent_records_nothing() {
        let t = tracer(10, 100);
        let s = t.scoped(None, "tool", "orphan");
        assert!(s.ctx().is_none());
        s.ok();
        assert_eq!(t.list()["kept"], 0);
    }

    #[test]
    fn old_traces_are_evicted_and_spans_are_capped() {
        let t = tracer(2, 3);
        let first = t.begin(None, None, "agent", "one").unwrap();
        t.begin(None, None, "agent", "two").unwrap();
        let third = t.begin(None, None, "agent", "three").unwrap();
        assert!(t.get(&first.trace_id).is_none());
        assert_eq!(t.list()["traces"][0]["name"], "three");
        assert_eq!(t.latest().as_deref(), Some(&*third.trace_id));
        for i in 0..5 {
            let child = t.begin(Some(&third), None, "tool", &format!("t{i}")).unwrap();
            t.end(&child, SpanStatus::Ok, None, vec![]);
        }
        let got = t.get(&third.trace_id).unwrap();
        assert_eq!(got["spans"].as_array().unwrap().len(), 3);
        assert_eq!(got["dropped_spans"], 3);
        // Ending an evicted span is harmless.
        t.end(&first, SpanStatus::Ok, None, vec![]);
    }

    #[test]
    fn a_valid_traceparent_is_joined_and_a_bad_one_is_ignored() {
        let t = tracer(10, 100);
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let root = t.begin(None, Some(header), "agent", "a").unwrap();
        assert_eq!(&*root.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        let got = t.get(&root.trace_id).unwrap();
        assert_eq!(got["spans"][0]["parent_span_id"], "00f067aa0ba902b7");
        assert_eq!(got["spans"][0]["remote_parent"], true);
        assert!(root.traceparent().starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));

        for bad in ["", "garbage", "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
                    "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
                    "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"] {
            assert!(parse_traceparent(bad).is_none(), "{bad}");
        }
        let fresh = t.begin(None, Some("garbage"), "agent", "b").unwrap();
        assert_ne!(fresh.trace_id, root.trace_id);
        assert_eq!(fresh.trace_id.len(), 32);
    }

    #[test]
    fn disabled_tracing_opens_nothing() {
        let t = Tracer::new(&TracingConfig { enabled: false, ..Default::default() });
        assert!(t.begin(None, None, "agent", "a").is_none());
        assert_eq!(t.list()["enabled"], false);
        assert!(t.export_target(|_| Some("http://collector:4318".into())).is_none());
    }

    #[test]
    fn the_export_target_comes_from_the_manifest_then_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        let declared = Tracer::new(&TracingConfig {
            otlp_endpoint: Some("http://otel:4318/".into()),
            otlp_headers_env: Some("MY_HEADERS".into()),
            ..Default::default()
        });
        let target = declared
            .export_target(env(&[("MY_HEADERS", "authorization=Bearer x, x-team = core ,bad"),
                                 ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://ignored:4318")]))
            .unwrap();
        assert_eq!(target.url, "http://otel:4318/v1/traces");
        assert_eq!(target.headers, vec![("authorization".to_string(), "Bearer x".to_string()),
                                        ("x-team".to_string(), "core".to_string())]);

        let plain = tracer(10, 10);
        assert!(plain.export_target(env(&[])).is_none());
        assert_eq!(plain.export_target(env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "https://c.example")])).unwrap().url,
                   "https://c.example/v1/traces");
        assert_eq!(plain.export_target(env(&[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "https://c.example/custom"),
                                             ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://other")])).unwrap().url,
                   "https://c.example/custom");
        assert!(plain.export_target(env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "collector:4317")])).is_none());
    }

    #[test]
    fn the_otlp_body_has_the_shape_a_collector_reads() {
        let t = tracer(10, 10);
        let root = t.begin(None, None, "agent", "agent a").unwrap();
        let mut m = t.scoped(Some(&root), "model", "chat m");
        m.set("gen_ai.usage.input_tokens", json!(7));
        m.set("gen_ai.request.model", json!("m"));
        m.fail("WC304");
        t.end(&root, SpanStatus::Ok, None, vec![]);
        let traces = t.traces.lock().unwrap();
        let batch: Vec<ExportSpan> = traces[0]
            .spans
            .iter()
            .map(|s| ExportSpan { trace_id: traces[0].trace_id.clone(), span: s.clone() })
            .collect();
        let body = otlp_body(&batch, "shop", "1.2.3");
        let resource = &body["resourceSpans"][0];
        assert_eq!(resource["resource"]["attributes"][0]["value"]["stringValue"], "shop");
        let spans = resource["scopeSpans"][0]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(spans[0]["spanId"].as_str().unwrap().len(), 16);
        assert!(spans[0].get("parentSpanId").is_none());
        assert_eq!(spans[0]["status"]["code"], 1);
        assert_eq!(spans[1]["parentSpanId"], spans[0]["spanId"]);
        assert_eq!(spans[1]["kind"], 3);
        assert_eq!(spans[1]["status"], json!({"code": 2, "message": "WC304"}));
        assert!(spans[1]["startTimeUnixNano"].is_string());
        let attrs = spans[1]["attributes"].as_array().unwrap();
        assert!(attrs.contains(&json!({"key": "gen_ai.usage.input_tokens", "value": {"intValue": "7"}})));
        assert!(attrs.contains(&json!({"key": "gen_ai.request.model", "value": {"stringValue": "m"}})));
        assert!(attrs.contains(&json!({"key": "webcortex.error.code", "value": {"stringValue": "WC304"}})));
    }
}
