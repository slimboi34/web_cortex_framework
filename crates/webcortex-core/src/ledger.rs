//! The spend ledger.
//!
//! Every provider call is charged here, keyed by what made it (an agent, a
//! behaviour, a flow's classifier) and which model answered. It is what
//! `GET /_webcortex/usage` reads, and it is in-process and bounded on purpose:
//! it answers "what is this app spending, right now, on what" without a metrics
//! pipeline. Ship the audit log for history.

use crate::manifest::Price;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Totals {
    fn add(&mut self, input: u64, output: u64, cache_read: u64, cache_write: u64) {
        self.merge(&Totals {
            calls: 1,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        });
    }

    /// Fold `other` into these totals. Saturating: the token counts are
    /// whatever an upstream's `usage` field reported.
    fn merge(&mut self, other: &Totals) {
        self.calls = self.calls.saturating_add(other.calls);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.saturating_add(other.cache_write_tokens);
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    pub fn cost_usd(&self, price: &Price) -> f64 {
        (self.input_tokens as f64 * price.input_per_mtok
            + self.output_tokens as f64 * price.output_per_mtok
            + self.cache_read_tokens as f64 * price.cache_read_per_mtok
            + self.cache_write_tokens as f64 * price.cache_write_per_mtok)
            / 1_000_000.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct LedgerKey {
    /// `agent`, `behaviour`, `flow`, `compaction`
    pub kind: String,
    pub name: String,
    pub model: String,
}

pub struct Ledger {
    entries: Mutex<BTreeMap<LedgerKey, Totals>>,
    runs: Mutex<u64>,
    started: Instant,
    started_unix: u64,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(0),
            started: Instant::now(),
            started_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

impl Ledger {
    /// The four token kinds are distinct billing lines, and folding them into
    /// a struct would only move the same seven values one level down.
    #[allow(clippy::too_many_arguments)]
    pub fn charge(
        &self,
        kind: &str,
        name: &str,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) {
        let key = LedgerKey { kind: kind.into(), name: name.into(), model: model.into() };
        let mut entries = match self.entries.lock() {
            Ok(e) => e,
            Err(p) => p.into_inner(),
        };
        // Bounded: a runaway of distinct (kind, name, model) keys cannot grow
        // memory without limit. Real apps have a handful.
        if entries.len() >= 10_000 && !entries.contains_key(&key) {
            return;
        }
        entries.entry(key).or_default().add(input, output, cache_read, cache_write);
    }

    pub fn count_run(&self) {
        if let Ok(mut r) = self.runs.lock() {
            *r += 1;
        }
    }

    /// A report for the control plane. `pricing` is consulted per model; where
    /// no price is known the cost is reported as null rather than as zero.
    pub fn snapshot(&self, price_for: impl Fn(&str) -> Option<Price>) -> serde_json::Value {
        let entries = match self.entries.lock() {
            Ok(e) => e.clone(),
            Err(p) => p.into_inner().clone(),
        };
        let runs = self.runs.lock().map(|r| *r).unwrap_or(0);

        let mut total = Totals::default();
        let mut by_model: BTreeMap<String, Totals> = BTreeMap::new();
        let mut by_caller: BTreeMap<String, Totals> = BTreeMap::new();
        let mut cost: Option<f64> = Some(0.0);
        let mut unpriced: Vec<String> = Vec::new();

        for (k, t) in &entries {
            total.merge(t);
            by_model.entry(k.model.clone()).or_default().merge(t);
            by_caller.entry(format!("{}:{}", k.kind, k.name)).or_default().merge(t);

            match price_for(&k.model) {
                Some(p) => {
                    if let Some(c) = cost.as_mut() {
                        *c += t.cost_usd(&p);
                    }
                }
                None => {
                    cost = None;
                    if !unpriced.contains(&k.model) {
                        unpriced.push(k.model.clone());
                    }
                }
            }
        }

        let priced_cost: f64 = entries
            .iter()
            .filter_map(|(k, t)| price_for(&k.model).map(|p| t.cost_usd(&p)))
            .sum();

        serde_json::json!({
            "since_unix": self.started_unix,
            "uptime_secs": self.started.elapsed().as_secs(),
            "runs": runs,
            "totals": total,
            "total_tokens": total.total_tokens(),
            "by_model": by_model,
            "by_caller": by_caller,
            // `estimated_cost_usd` is exact only when every model is priced.
            // Otherwise it is null and `priced_cost_usd` shows the priced part.
            "estimated_cost_usd": cost,
            "priced_cost_usd": priced_cost,
            "unpriced_models": unpriced,
            "entries": entries.iter().map(|(k, t)| serde_json::json!({
                "kind": k.kind, "name": k.name, "model": k.model, "usage": t,
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_accumulate_by_key() {
        let l = Ledger::default();
        l.charge("agent", "a", "m", 10, 5, 0, 0);
        l.charge("agent", "a", "m", 10, 5, 3, 0);
        l.charge("behaviour", "b", "m2", 1, 1, 0, 0);
        let snap = l.snapshot(|_| None);
        assert_eq!(snap["totals"]["calls"], 3);
        assert_eq!(snap["totals"]["input_tokens"], 21);
        assert_eq!(snap["by_caller"]["agent:a"]["cache_read_tokens"], 3);
        assert!(snap["estimated_cost_usd"].is_null(), "unpriced must not be reported as free");
        assert_eq!(snap["unpriced_models"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn cost_is_computed_when_every_model_is_priced() {
        let l = Ledger::default();
        l.charge("agent", "a", "m", 1_000_000, 1_000_000, 0, 0);
        let snap = l.snapshot(|_| Some(Price { input_per_mtok: 1.0, output_per_mtok: 5.0, ..Default::default() }));
        assert_eq!(snap["estimated_cost_usd"], 6.0);
    }
}
