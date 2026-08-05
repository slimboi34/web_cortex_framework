//! The audit trail.
//!
//! Every agent action is recorded — including the ones that were refused, which
//! are usually the interesting ones. Recording is synchronous and infallible
//! from the caller's perspective: an audit sink must never be able to fail a
//! request, and must never be skippable on the error path.

use crate::agent::{RunStatus, Usage};
use crate::auth::Principal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub kind: String,
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub detail: Value,
}

impl AuditEvent {
    pub fn agent_started(
        run_id: &str,
        agent: &str,
        actor: &Principal,
        tools: &[crate::agent::ToolSpec],
    ) -> Self {
        Self {
            kind: "agent_started".into(),
            run_id: run_id.into(),
            actor: Some(actor.id.clone()),
            tool: None,
            detail: json!({
                "agent": agent,
                // The granted scope set is the security-relevant fact worth
                // capturing; it proves what the run *could* have done.
                "granted_scopes": actor.scopes,
                "tools": tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
            }),
        }
    }

    pub fn agent_finished(run_id: &str, agent: &str, status: RunStatus, usage: &Usage) -> Self {
        Self {
            kind: "agent_finished".into(),
            run_id: run_id.into(),
            actor: None,
            tool: None,
            detail: json!({
                "agent": agent,
                "status": status,
                "steps": usage.steps,
                "tool_calls": usage.tool_calls,
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            }),
        }
    }

    pub fn agent_failed(run_id: &str, agent: &str, error: &str) -> Self {
        Self {
            kind: "agent_failed".into(),
            run_id: run_id.into(),
            actor: None,
            tool: None,
            detail: json!({"agent": agent, "error": error}),
        }
    }

    pub fn tool_called(
        run_id: &str,
        actor: &Principal,
        tool: &str,
        arguments: &Value,
        ok: bool,
        duration_ms: u64,
    ) -> Self {
        Self {
            kind: "tool_called".into(),
            run_id: run_id.into(),
            actor: Some(actor.id.clone()),
            tool: Some(tool.into()),
            detail: json!({"arguments": arguments, "ok": ok, "duration_ms": duration_ms}),
        }
    }

    pub fn tool_refused(run_id: &str, actor: &Principal, tool: &str, reason: &str) -> Self {
        Self {
            kind: "tool_refused".into(),
            run_id: run_id.into(),
            actor: Some(actor.id.clone()),
            tool: Some(tool.into()),
            detail: json!({"reason": reason}),
        }
    }

    pub fn approval_requested(
        run_id: &str,
        actor: &Principal,
        tool: &str,
        arguments: &Value,
        approval_id: &str,
    ) -> Self {
        Self {
            kind: "approval_requested".into(),
            run_id: run_id.into(),
            actor: Some(actor.id.clone()),
            tool: Some(tool.into()),
            detail: json!({"arguments": arguments, "approval_id": approval_id}),
        }
    }
}

pub trait AuditSink: Send + Sync + 'static {
    fn record(&self, event: AuditEvent);
    /// Most recent events, newest last. For the control-plane endpoint.
    fn recent(&self, _limit: usize) -> Vec<AuditEvent> {
        Vec::new()
    }
}

/// Writes events to the tracing subscriber. The default: audit output lands
/// wherever the operator already collects logs, with no extra infrastructure.
pub struct TracingAudit;

impl AuditSink for TracingAudit {
    fn record(&self, event: AuditEvent) {
        tracing::info!(
            target: "rango::audit",
            kind = %event.kind,
            run_id = %event.run_id,
            actor = event.actor.as_deref().unwrap_or("-"),
            tool = event.tool.as_deref().unwrap_or("-"),
            detail = %event.detail,
            "audit"
        );
    }
}

/// Keeps a bounded in-memory ring, in addition to logging. Powers
/// `GET /_rango/audit` so an operator can see agent activity without shipping
/// logs anywhere first.
pub struct MemoryAudit {
    events: Mutex<std::collections::VecDeque<AuditEvent>>,
    capacity: usize,
}

impl Default for MemoryAudit {
    fn default() -> Self {
        Self::with_capacity(1000)
    }
}

impl MemoryAudit {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Mutex::new(std::collections::VecDeque::with_capacity(capacity.min(256))),
            capacity,
        }
    }

    #[cfg(test)]
    pub fn contains(&self, kind: &str) -> bool {
        self.events
            .lock()
            .map(|e| e.iter().any(|ev| ev.kind == kind))
            .unwrap_or(false)
    }
}

impl AuditSink for MemoryAudit {
    fn record(&self, event: AuditEvent) {
        TracingAudit.record(event.clone());
        let mut events = match self.events.lock() {
            Ok(e) => e,
            Err(p) => p.into_inner(),
        };
        if events.len() >= self.capacity {
            events.pop_front();
        }
        events.push_back(event);
    }

    fn recent(&self, limit: usize) -> Vec<AuditEvent> {
        let events = match self.events.lock() {
            Ok(e) => e,
            Err(p) => p.into_inner(),
        };
        events.iter().rev().take(limit).rev().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str) -> AuditEvent {
        AuditEvent {
            kind: kind.into(),
            run_id: "r".into(),
            actor: None,
            tool: None,
            detail: Value::Null,
        }
    }

    #[test]
    fn recent_returns_events_oldest_first() {
        let a = MemoryAudit::default();
        a.record(event("one"));
        a.record(event("two"));
        let recent = a.recent(10);
        assert_eq!(recent[0].kind, "one");
        assert_eq!(recent[1].kind, "two");
    }

    #[test]
    fn the_ring_is_bounded() {
        let a = MemoryAudit::with_capacity(3);
        for i in 0..10 {
            a.record(event(&format!("e{i}")));
        }
        let recent = a.recent(100);
        assert_eq!(recent.len(), 3, "must not grow without bound");
        assert_eq!(recent[2].kind, "e9", "newest event must survive");
    }

    #[test]
    fn limit_takes_the_newest_events() {
        let a = MemoryAudit::default();
        for i in 0..5 {
            a.record(event(&format!("e{i}")));
        }
        let recent = a.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[1].kind, "e4");
    }

    #[test]
    fn started_event_captures_granted_scopes_for_forensics() {
        let p = Principal {
            id: "agent:a#u1".into(),
            kind: crate::auth::PrincipalKind::Agent,
            scopes: vec!["read".into()],
            claims: Default::default(),
        };
        let ev = AuditEvent::agent_started("r", "a", &p, &[]);
        assert_eq!(ev.detail["granted_scopes"], json!(["read"]));
    }
}
