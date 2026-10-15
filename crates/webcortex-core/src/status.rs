//! The live system view: what is this app doing right now?
//!
//! [`Recorder`] lives on the [`App`] and is fed from three places: the HTTP
//! edge (one call per request), the agent runtime (start, each model call,
//! finish), and the watchers. [`snapshot`] turns it, together with the
//! ledger, the e-stop, the device hub and the approvals, into the document
//! served at `GET /_webcortex/status`, returned by the built-in
//! `webcortex_status` MCP tool, and read by `webcortex status`.
//!
//! The request path is lock-free: a route's counters are relaxed atomics in a
//! slot found by route id, and the latency histogram is 40 power-of-two
//! buckets. Only failures (into a bounded ring) and agent runs (which are
//! already measured in seconds) take a lock. Nothing here stores a request or
//! response body, a header, a query string or a credential.

use crate::app::App;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How many recent failures and finished runs are kept.
pub const RECENT_ERRORS: usize = 100;
pub const RECENT_RUNS: usize = 50;
const BUCKETS: usize = 40;

pub fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Latency in power-of-two microsecond buckets: bucket `i` holds samples in
/// `[2^(i-1), 2^i)` µs. Percentiles are reported as the bucket's upper bound,
/// so they are never optimistic and within 2x of the truth — capped at the
/// largest sample actually seen, so a percentile never exceeds `max_ms`.
struct Histogram {
    buckets: [AtomicU64; BUCKETS],
}

impl Histogram {
    fn new() -> Self {
        Self { buckets: std::array::from_fn(|_| AtomicU64::new(0)) }
    }

    fn record(&self, us: u64) {
        let i = (u64::BITS - us.leading_zeros()) as usize;
        self.buckets[i.min(BUCKETS - 1)].fetch_add(1, Relaxed);
    }

    fn percentile_ms(&self, counts: &[u64; BUCKETS], total: u64, p: f64, max_us: u64) -> f64 {
        if total == 0 {
            return 0.0;
        }
        let target = ((total as f64) * p).ceil().max(1.0) as u64;
        let mut seen = 0;
        let mut upper = 1u64 << (BUCKETS - 1);
        for (i, c) in counts.iter().enumerate() {
            seen += c;
            if seen >= target {
                upper = 1u64 << i;
                break;
            }
        }
        // A bucket's upper bound can lie above every sample in it; the true
        // maximum is known exactly, and no percentile can exceed it.
        round3(upper.min(max_us) as f64 / 1000.0)
    }

    /// p50/p95/p99, none of them above `max_us`, the largest sample recorded.
    fn summary(&self, max_us: u64) -> Value {
        let counts: [u64; BUCKETS] = std::array::from_fn(|i| self.buckets[i].load(Relaxed));
        let total: u64 = counts.iter().sum();
        json!({
            "p50_ms": self.percentile_ms(&counts, total, 0.50, max_us),
            "p95_ms": self.percentile_ms(&counts, total, 0.95, max_us),
            "p99_ms": self.percentile_ms(&counts, total, 0.99, max_us),
        })
    }
}

/// One failure in the recent-errors ring. Plain fields, not JSON: building a
/// `serde_json::Value` on every failed request cost measurable throughput
/// under a flood of 404s, so the JSON is built only when someone reads it.
struct ErrorRecord {
    at_ms: u64,
    source: &'static str,
    route: Arc<str>,
    status: Option<u16>,
    code: Arc<str>,
    request_id: Box<str>,
}

impl ErrorRecord {
    fn to_json(&self) -> Value {
        json!({
            "at_ms": self.at_ms,
            "source": self.source,
            "route": &*self.route,
            "status": self.status,
            "code": &*self.code,
            "request_id": &*self.request_id,
        })
    }
}

/// A code as a shared string: catalogued codes are interned once, so recording
/// one allocates nothing.
fn shared_code(code: &str) -> Arc<str> {
    static INTERNED: std::sync::OnceLock<HashMap<&'static str, Arc<str>>> = std::sync::OnceLock::new();
    let table = INTERNED.get_or_init(|| crate::errors::CATALOGUE.iter().map(|e| (e.code, Arc::from(e.code))).collect());
    table.get(code).cloned().unwrap_or_else(|| Arc::from(code))
}

/// One route's counters.
struct Slot {
    label: Arc<str>,
    count: AtomicU64,
    /// 1xx … 5xx
    by_class: [AtomicU64; 5],
    total_us: AtomicU64,
    max_us: AtomicU64,
    latency: Histogram,
}

impl Slot {
    fn new(label: String) -> Self {
        Self {
            label: Arc::from(label),
            count: AtomicU64::new(0),
            by_class: std::array::from_fn(|_| AtomicU64::new(0)),
            total_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            latency: Histogram::new(),
        }
    }

    fn record(&self, status: u16, us: u64) {
        self.count.fetch_add(1, Relaxed);
        let class = (status / 100).clamp(1, 5) as usize - 1;
        self.by_class[class].fetch_add(1, Relaxed);
        self.total_us.fetch_add(us, Relaxed);
        self.max_us.fetch_max(us, Relaxed);
        self.latency.record(us);
    }

    fn describe(&self) -> Option<Value> {
        let count = self.count.load(Relaxed);
        if count == 0 {
            return None;
        }
        let total = self.total_us.load(Relaxed);
        let max_us = self.max_us.load(Relaxed);
        let mut v = json!({
            "route": &*self.label,
            "requests": count,
            "status": {
                "1xx": self.by_class[0].load(Relaxed),
                "2xx": self.by_class[1].load(Relaxed),
                "3xx": self.by_class[2].load(Relaxed),
                "4xx": self.by_class[3].load(Relaxed),
                "5xx": self.by_class[4].load(Relaxed),
            },
            "mean_ms": round3(total as f64 / count as f64 / 1000.0),
            "max_ms": round3(max_us as f64 / 1000.0),
        });
        if let (Value::Object(m), Value::Object(l)) = (&mut v, self.latency.summary(max_us)) {
            m.extend(l);
        }
        Some(v)
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Where a request is counted.
pub enum Target {
    Route(u32),
    Control,
    Unmatched,
}

struct LiveRun {
    /// The agent the run started as; `agent` follows handoffs.
    entry: String,
    agent: String,
    caller: String,
    depth: u32,
    started: Instant,
    started_unix_ms: u64,
    steps: u32,
    tokens: u64,
}

#[derive(Default, Clone, serde::Serialize)]
struct AgentTally {
    runs: u64,
    completed: u64,
    failed: u64,
    budget_exhausted: u64,
    step_limit: u64,
    max_tokens: u64,
    awaiting_approval: u64,
    /// Runs abandoned before they finished: the request timed out, or the
    /// client went away and the run was dropped with it.
    cancelled: u64,
    tokens: u64,
    largest_run_tokens: u64,
}

#[derive(Default)]
struct RunBook {
    in_flight: HashMap<String, LiveRun>,
    recent: VecDeque<Value>,
    by_agent: HashMap<String, AgentTally>,
}

#[derive(Default, Clone, serde::Serialize)]
struct WatcherTally {
    runs: u64,
    failed: u64,
    skipped_errors: u64,
    last_status: Option<String>,
    last_code: Option<String>,
    last_at_ms: Option<u64>,
}

/// What a finished run reports to the recorder.
pub struct RunSummary<'a> {
    pub run_id: &'a str,
    pub agent: &'a str,
    pub path: &'a [String],
    pub status: &'a str,
    pub steps: u32,
    pub tool_calls: u32,
    pub tokens: u64,
    pub tree_tokens: u64,
    pub cost_usd: Option<f64>,
    pub code: Option<&'a str>,
}

pub struct Recorder {
    started: Instant,
    started_unix_ms: u64,
    slots: Vec<Slot>,
    by_route: HashMap<u32, usize>,
    control: usize,
    unmatched: usize,
    all: Slot,
    errors_total: AtomicU64,
    errors: Mutex<VecDeque<ErrorRecord>>,
    runs: Mutex<RunBook>,
    watchers: Mutex<HashMap<String, WatcherTally>>,
}

impl Recorder {
    pub fn new(manifest: &crate::manifest::Manifest) -> Self {
        let mut slots = Vec::with_capacity(manifest.routes.len() + 2);
        let mut by_route = HashMap::new();
        for r in &manifest.routes {
            by_route.insert(r.id, slots.len());
            slots.push(Slot::new(format!("{} {}", r.method, r.path)));
        }
        let control = slots.len();
        slots.push(Slot::new(format!("{}/*", manifest.server.control_prefix)));
        let unmatched = slots.len();
        slots.push(Slot::new("(no route)".into()));
        Self {
            started: Instant::now(),
            started_unix_ms: unix_ms(),
            slots,
            by_route,
            control,
            unmatched,
            all: Slot::new("*".into()),
            errors_total: AtomicU64::new(0),
            errors: Mutex::new(VecDeque::new()),
            runs: Mutex::new(RunBook::default()),
            watchers: Mutex::new(HashMap::new()),
        }
    }

    /// Count one request. Lock-free unless it failed.
    #[allow(clippy::too_many_arguments)]
    pub fn record_request(
        &self,
        target: Target,
        method: &str,
        path: &str,
        status: u16,
        elapsed_us: u64,
        request_id: &str,
        code: Option<&str>,
    ) {
        let slot = match target {
            Target::Route(id) => self.by_route.get(&id).copied().unwrap_or(self.unmatched),
            Target::Control => self.control,
            Target::Unmatched => self.unmatched,
        };
        self.slots[slot].record(status, elapsed_us);
        self.all.record(status, elapsed_us);
        if status >= 400 {
            // The route pattern, not the concrete path, wherever one matched:
            // a path can carry an id, and the pattern is what to look at.
            let route = if slot == self.unmatched || slot == self.control {
                Arc::from(format!("{method} {}", truncate(path, 96)))
            } else {
                self.slots[slot].label.clone()
            };
            self.error(ErrorRecord {
                at_ms: unix_ms(),
                source: "http",
                route,
                status: Some(status),
                code: shared_code(code.unwrap_or_else(|| crate::errors::code_for_status(status))),
                request_id: request_id.into(),
            });
        }
    }

    /// Record a failure that did not end an HTTP request: an MCP tool error,
    /// a watcher that could not run. Same ring as HTTP failures.
    pub fn record_failure(&self, source: &'static str, route: &str, code: &str, request_id: &str) {
        self.error(ErrorRecord {
            at_ms: unix_ms(),
            source,
            route: Arc::from(truncate(route, 128)),
            status: None,
            code: shared_code(code),
            request_id: request_id.into(),
        });
    }

    fn error(&self, record: ErrorRecord) {
        self.errors_total.fetch_add(1, Relaxed);
        let evicted = {
            let mut ring = self.errors.lock().unwrap_or_else(|p| p.into_inner());
            let evicted = if ring.len() >= RECENT_ERRORS { ring.pop_front() } else { None };
            ring.push_back(record);
            evicted
        };
        // Freed outside the lock.
        drop(evicted);
    }

    pub fn run_started(&self, run_id: &str, agent: &str, caller: &str, depth: u32) {
        let mut book = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        // Bounded like everything else here: a flood of runs that never finish
        // (a hung provider) must not grow this without limit.
        if book.in_flight.len() >= 10_000 {
            return;
        }
        book.in_flight.insert(
            run_id.to_string(),
            LiveRun {
                entry: agent.to_string(),
                agent: agent.to_string(),
                caller: caller.to_string(),
                depth,
                started: Instant::now(),
                started_unix_ms: unix_ms(),
                steps: 0,
                tokens: 0,
            },
        );
    }

    pub fn run_progress(&self, run_id: &str, agent: &str, steps: u32, tokens: u64) {
        let mut book = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = book.in_flight.get_mut(run_id) {
            r.agent = agent.to_string();
            r.steps = steps;
            r.tokens = tokens;
        }
    }

    pub fn run_finished(&self, s: RunSummary<'_>) {
        let mut book = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let live = book.in_flight.remove(s.run_id);
        let duration_ms = live.as_ref().map(|r| r.started.elapsed().as_millis() as u64);
        let entry_agent = s.path.first().map(String::as_str).unwrap_or(s.agent).to_string();
        let tally = book.by_agent.entry(entry_agent).or_default();
        tally.runs += 1;
        match s.status {
            "completed" => tally.completed += 1,
            "failed" => tally.failed += 1,
            "budget_exhausted" => tally.budget_exhausted += 1,
            "step_limit" => tally.step_limit += 1,
            "max_tokens" => tally.max_tokens += 1,
            "awaiting_approval" => tally.awaiting_approval += 1,
            _ => {}
        }
        tally.tokens = tally.tokens.saturating_add(s.tokens);
        tally.largest_run_tokens = tally.largest_run_tokens.max(s.tokens);
        if book.recent.len() >= RECENT_RUNS {
            book.recent.pop_front();
        }
        book.recent.push_back(json!({
            "run_id": s.run_id,
            "agent": s.agent,
            "path": s.path,
            "caller": live.as_ref().map(|r| r.caller.clone()),
            "depth": live.as_ref().map(|r| r.depth),
            "outcome": s.status,
            "code": s.code,
            "steps": s.steps,
            "tool_calls": s.tool_calls,
            "tokens": s.tokens,
            "tree_tokens": s.tree_tokens,
            "cost_usd": s.cost_usd,
            "duration_ms": duration_ms,
            "finished_at_ms": unix_ms(),
        }));
        drop(book);
        if let Some(code) = s.code
            && s.status == "failed" {
                self.record_failure("agent", &format!("agent {}", s.agent), code, s.run_id);
            }
    }

    /// A run that was dropped before it finished: its request timed out or its
    /// client disconnected, and the future driving it went with them. Without
    /// this it stayed "in flight" for the life of the process.
    pub fn run_cancelled(&self, run_id: &str) {
        let mut book = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let Some(live) = book.in_flight.remove(run_id) else { return };
        let tally = book.by_agent.entry(live.entry.clone()).or_default();
        tally.runs += 1;
        tally.cancelled += 1;
        tally.tokens = tally.tokens.saturating_add(live.tokens);
        tally.largest_run_tokens = tally.largest_run_tokens.max(live.tokens);
        if book.recent.len() >= RECENT_RUNS {
            book.recent.pop_front();
        }
        book.recent.push_back(json!({
            "run_id": run_id,
            "agent": live.agent,
            "path": [live.entry],
            "caller": live.caller,
            "depth": live.depth,
            "outcome": "cancelled",
            "code": null,
            "steps": live.steps,
            "tool_calls": null,
            "tokens": live.tokens,
            "tree_tokens": null,
            "cost_usd": null,
            "duration_ms": live.started.elapsed().as_millis() as u64,
            "finished_at_ms": unix_ms(),
        }));
    }

    pub fn watcher_run(&self, name: &str, status: &str, code: Option<&str>) {
        let mut w = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        let t = w.entry(name.to_string()).or_default();
        t.runs += 1;
        if status == "failed" {
            t.failed += 1;
        }
        t.last_status = Some(status.to_string());
        t.last_code = code.map(str::to_string);
        t.last_at_ms = Some(unix_ms());
    }

    pub fn watcher_error(&self, name: &str, code: &str) {
        {
            let mut w = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
            let t = w.entry(name.to_string()).or_default();
            t.skipped_errors += 1;
            t.last_code = Some(code.to_string());
            t.last_at_ms = Some(unix_ms());
        }
        self.record_failure("watcher", &format!("watcher {name}"), code, "");
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn requests(&self) -> Value {
        let mut routes: Vec<Value> = self.slots.iter().filter_map(Slot::describe).collect();
        routes.sort_by_key(|r| std::cmp::Reverse(r["requests"].as_u64().unwrap_or(0)));
        json!({
            "total": self.all.describe().unwrap_or_else(|| json!({"requests": 0})),
            "routes": routes,
        })
    }

    fn errors(&self) -> Value {
        let ring = self.errors.lock().unwrap_or_else(|p| p.into_inner());
        json!({
            "total": self.errors_total.load(Relaxed),
            "recent": ring.iter().rev().map(ErrorRecord::to_json).collect::<Vec<_>>(),
        })
    }

    fn runs(&self) -> (Value, HashMap<String, AgentTally>) {
        let book = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let mut in_flight: Vec<Value> = book
            .in_flight
            .iter()
            .map(|(id, r)| {
                json!({
                    "run_id": id,
                    "agent": r.agent,
                    "caller": r.caller,
                    "depth": r.depth,
                    "steps": r.steps,
                    "tokens": r.tokens,
                    "started_at_ms": r.started_unix_ms,
                    "running_ms": r.started.elapsed().as_millis() as u64,
                })
            })
            .collect();
        in_flight.sort_by_key(|r| r["started_at_ms"].as_u64().unwrap_or(0));
        (
            json!({
                "in_flight": in_flight,
                "recent": book.recent.iter().rev().cloned().collect::<Vec<_>>(),
            }),
            book.by_agent.clone(),
        )
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The whole live view, as served at `GET {control_prefix}/status`.
pub fn snapshot(app: &App) -> Value {
    let rec = app.recorder();
    let (runs, tallies) = rec.runs();
    let usage = app.ledger().snapshot(|m| app.registry().price_for(m));
    let by_caller = usage.get("by_caller").cloned().unwrap_or(Value::Null);
    let spent = |kind: &str, name: &str| -> u64 {
        by_caller
            .get(format!("{kind}:{name}"))
            .map(|t| {
                ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens"]
                    .iter()
                    .map(|k| t[*k].as_u64().unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0)
    };

    let m = &app.manifest;
    let mut budgets = Vec::new();
    for a in &m.agents {
        let t = tallies.get(&a.name).cloned().unwrap_or_default();
        budgets.push(json!({
            "kind": "agent",
            "name": a.name,
            "token_budget": a.token_budget,
            "max_steps": a.max_steps,
            "tokens_spent": spent("agent", &a.name),
            "runs": t.runs,
            "largest_run_tokens": t.largest_run_tokens,
            "budget_exhausted": t.budget_exhausted,
            "step_limit": t.step_limit,
            "failed": t.failed,
        }));
    }
    for b in &m.behaviours {
        budgets.push(json!({
            "kind": "behaviour",
            "name": b.name,
            "token_budget": b.token_budget,
            "max_steps": b.max_steps,
            "tokens_spent": spent("behaviour", &b.name),
        }));
    }
    for f in &m.flows {
        budgets.push(json!({
            "kind": "flow",
            "name": f.name,
            "token_budget": f.token_budget,
            "tokens_spent": spent("flow", &f.name),
        }));
    }

    let watchers = rec.watchers.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let devices = app.devices().stats();

    json!({
        "app": m.name,
        "app_version": m.version,
        "webcortex": env!("CARGO_PKG_VERSION"),
        "now_ms": unix_ms(),
        "started_at_ms": rec.started_unix_ms,
        "uptime_secs": rec.uptime_secs(),
        "python_workers": app.bridge().workers(),
        "requests": rec.requests(),
        "errors": rec.errors(),
        "agents": {
            "in_flight": runs["in_flight"],
            "recent": runs["recent"],
            "by_agent": tallies,
            "sessions": app.sessions().len(),
            "pending_approvals": app.pending_approvals().len(),
        },
        "usage": {
            "runs": usage["runs"],
            "total_tokens": usage["total_tokens"],
            "estimated_cost_usd": usage["estimated_cost_usd"],
            "priced_cost_usd": usage["priced_cost_usd"],
            "unpriced_models": usage["unpriced_models"],
            "by_model": usage["by_model"],
        },
        "budgets": budgets,
        "estop": {
            "halted": app.halted().is_some(),
            "state": app.halted(),
            "actuators": m.routes.iter().filter(|r| r.actuator).map(|r| r.tool_name()).collect::<Vec<_>>(),
        },
        "devices": {
            "devices": devices,
            "watchers": m.watchers.iter().map(|w| json!({
                "name": w.name,
                "device": w.device,
                "agent": w.agent,
                "every_secs": w.every_secs,
                "stats": watchers.get(&w.name).cloned().unwrap_or_default(),
            })).collect::<Vec<_>>(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> crate::manifest::Manifest {
        serde_json::from_value(json!({
            "name": "t",
            "routes": [{"id": 7, "method": "GET", "path": "/a/{id}", "op": {"kind": "static", "body": {}}}]
        }))
        .unwrap()
    }

    #[test]
    fn requests_are_counted_by_route_pattern_and_class() {
        let r = Recorder::new(&manifest());
        r.record_request(Target::Route(7), "GET", "/a/1", 200, 120, "r1", None);
        r.record_request(Target::Route(7), "GET", "/a/2", 404, 80, "r2", Some("WC207"));
        r.record_request(Target::Unmatched, "GET", "/nope", 404, 10, "r3", Some("WC201"));
        let v = r.requests();
        assert_eq!(v["total"]["requests"], 3);
        let a = v["routes"].as_array().unwrap().iter().find(|x| x["route"] == "GET /a/{id}").unwrap();
        assert_eq!(a["requests"], 2);
        assert_eq!(a["status"]["2xx"], 1);
        assert_eq!(a["status"]["4xx"], 1);
        let errors = r.errors();
        assert_eq!(errors["total"], 2);
        // Newest first, the pattern rather than the concrete path where one matched.
        assert_eq!(errors["recent"][0]["route"], "GET /nope");
        assert_eq!(errors["recent"][1]["route"], "GET /a/{id}");
        assert_eq!(errors["recent"][1]["code"], "WC207");
        assert_eq!(errors["recent"][1]["request_id"], "r2");
    }

    #[test]
    fn the_error_ring_is_bounded() {
        let r = Recorder::new(&manifest());
        for i in 0..(RECENT_ERRORS + 25) {
            r.record_request(Target::Unmatched, "GET", "/x", 500, 1, &i.to_string(), None);
        }
        let e = r.errors();
        assert_eq!(e["total"], (RECENT_ERRORS + 25) as u64);
        assert_eq!(e["recent"].as_array().unwrap().len(), RECENT_ERRORS);
    }

    #[test]
    fn a_run_moves_from_in_flight_to_recent() {
        let r = Recorder::new(&manifest());
        r.run_started("run1", "desk", "alice", 0);
        r.run_progress("run1", "desk", 2, 40);
        let (runs, _) = r.runs();
        assert_eq!(runs["in_flight"][0]["tokens"], 40);
        r.run_finished(RunSummary {
            run_id: "run1", agent: "desk", path: &["desk".to_string()], status: "budget_exhausted",
            steps: 2, tool_calls: 1, tokens: 40, tree_tokens: 40, cost_usd: None, code: Some("WC402"),
        });
        let (runs, tallies) = r.runs();
        assert!(runs["in_flight"].as_array().unwrap().is_empty());
        assert_eq!(runs["recent"][0]["outcome"], "budget_exhausted");
        assert_eq!(runs["recent"][0]["code"], "WC402");
        assert_eq!(tallies["desk"].budget_exhausted, 1);
    }

    #[test]
    fn percentiles_are_upper_bounds() {
        let h = Histogram::new();
        for _ in 0..99 {
            h.record(100);
        }
        h.record(50_000);
        let s = h.summary(50_000);
        assert_eq!(s["p50_ms"], 0.128);
        assert!(s["p99_ms"].as_f64().unwrap() <= 0.128);
        assert!(s["p99_ms"].as_f64().unwrap() >= 0.1);
    }

    #[test]
    fn a_cancelled_run_leaves_in_flight_and_is_counted() {
        let r = Recorder::new(&manifest());
        r.run_started("run1", "desk", "alice", 0);
        r.run_progress("run1", "billing", 3, 120);
        r.run_cancelled("run1");
        r.run_cancelled("run1"); // a second drop is a no-op
        let (runs, tally) = r.runs();
        assert!(runs["in_flight"].as_array().unwrap().is_empty());
        assert_eq!(runs["recent"][0]["outcome"], "cancelled");
        assert_eq!(runs["recent"][0]["agent"], "billing");
        assert_eq!(runs["recent"][0]["tokens"], 120);
        assert_eq!(tally["desk"].cancelled, 1);
        assert_eq!(tally["desk"].runs, 1);
        // A run that finished normally is not touched by a late cancel.
        r.run_started("run2", "desk", "alice", 0);
        r.run_finished(RunSummary {
            run_id: "run2", agent: "desk", path: &["desk".to_string()], status: "completed",
            steps: 1, tool_calls: 0, tokens: 5, tree_tokens: 5, cost_usd: None, code: None,
        });
        r.run_cancelled("run2");
        let (_, tally) = r.runs();
        assert_eq!(tally["desk"].cancelled, 1);
        assert_eq!(tally["desk"].completed, 1);
    }

    #[test]
    fn a_percentile_of_zero_latency_samples_is_zero() {
        // The cap used max(1) µs, so all-zero samples reported p50 0.001 ms
        // against a max of 0.0.
        let slot = Slot::new("GET /".into());
        slot.record(200, 0);
        let v = slot.describe().unwrap();
        assert_eq!(v["max_ms"], 0.0);
        assert_eq!(v["p50_ms"], 0.0);
    }

    #[test]
    fn no_percentile_exceeds_the_maximum() {
        // 2.5.0 printed "p95 0.256ms  max 0.162ms": the bucket's upper bound
        // was reported even when every sample sat well below it.
        let slot = Slot::new("GET /".into());
        slot.record(200, 162);
        slot.record(200, 19);
        let v = slot.describe().unwrap();
        let max = v["max_ms"].as_f64().unwrap();
        assert_eq!(max, 0.162);
        for p in ["p50_ms", "p95_ms", "p99_ms"] {
            assert!(v[p].as_f64().unwrap() <= max, "{p} {} > max {max}", v[p]);
        }
        assert_eq!(v["p95_ms"], 0.162);
        assert_eq!(v["p50_ms"], 0.032);
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("ééé", 3), "é…");
    }
}
