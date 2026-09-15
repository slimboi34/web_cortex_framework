//! Model name resolution.
//!
//! A model name is resolved in two steps: aliases first (`fast` → a concrete
//! name), then a provider by prefix (`ollama/qwen3.5:9b` → the OpenAI-compatible
//! endpoint at `OLLAMA_HOST`). The prefix is stripped before the request is
//! sent, so a provider sees the name it actually serves.
//!
//! Aliases are the token-economy lever: a behaviour that classifies fifty
//! tickets should do it with `model="fast"`, and which model that is should be
//! decided once, in `app.models(...)`, not fifty times in the code.

use super::provider::{
    AnthropicProvider, CompletionRequest, FakeProvider, ModelProvider, OpenAiCompatProvider,
    ProviderResponse,
};
use crate::manifest::{AgentDef, ModelsConfig, Price};
use futures::future::BoxFuture;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const BUILTIN_DEFAULT: &str = "claude-opus-5";
pub const BUILTIN_FAST: &str = "claude-haiku-4-5-20251001";

pub struct ProviderRegistry {
    aliases: BTreeMap<String, String>,
    /// Ordered longest-prefix-first so `openrouter/` beats `open/`.
    prefixed: Vec<(String, Arc<dyn ModelProvider>)>,
    anthropic: Option<Arc<dyn ModelProvider>>,
    openai: Option<Arc<dyn ModelProvider>>,
    fake: Option<Arc<dyn ModelProvider>>,
    pricing: BTreeMap<String, Price>,
}

impl ProviderRegistry {
    /// Build from the manifest and the environment. Never fails: a missing key
    /// only makes the corresponding models unavailable, which is reported when
    /// one is asked for rather than at boot.
    pub fn from_env(cfg: &ModelsConfig) -> Self {
        let mut aliases = BTreeMap::new();
        aliases.insert("default".to_string(), BUILTIN_DEFAULT.to_string());
        aliases.insert("fast".to_string(), BUILTIN_FAST.to_string());
        for (k, v) in &cfg.aliases {
            aliases.insert(k.clone(), v.clone());
        }

        let fake = std::env::var("WEBCORTEX_FAKE_PROVIDER")
            .ok()
            .filter(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .map(|_| {
                tracing::warn!(
                    "WEBCORTEX_FAKE_PROVIDER is set: every model call is answered by a \
                     deterministic fake. This is for tests only."
                );
                Arc::new(FakeProvider) as Arc<dyn ModelProvider>
            });

        let anthropic = AnthropicProvider::from_env().map(|p| Arc::new(p) as Arc<dyn ModelProvider>);

        let openai = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .and_then(|key| {
                let base = std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into());
                OpenAiCompatProvider::new(base, Some(key), "openai")
            })
            .map(|p| Arc::new(p) as Arc<dyn ModelProvider>);

        let mut prefixed: Vec<(String, Arc<dyn ModelProvider>)> = Vec::new();

        // Ollama is built in because it is the local-model path most people
        // reach for first. No key, default host, override with OLLAMA_HOST.
        let ollama_host = std::env::var("OLLAMA_HOST")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:11434".into());
        let ollama_host = if ollama_host.starts_with("http") {
            ollama_host
        } else {
            format!("http://{ollama_host}")
        };
        if let Some(p) = OpenAiCompatProvider::new(
            format!("{}/v1", ollama_host.trim_end_matches('/')),
            None,
            "ollama",
        ) {
            prefixed.push(("ollama".into(), Arc::new(p)));
        }

        for (name, def) in &cfg.providers {
            let key = def
                .api_key_env
                .as_ref()
                .and_then(|env| std::env::var(env).ok())
                .filter(|k| !k.is_empty());
            if def.api_key_env.is_some() && key.is_none() {
                tracing::warn!(
                    provider = %name,
                    env = def.api_key_env.as_deref().unwrap_or("-"),
                    "provider api key env var is unset; models with this prefix will fail"
                );
            }
            let provider: Option<Arc<dyn ModelProvider>> = match def.kind.as_str() {
                "anthropic" => key
                    .and_then(|k| AnthropicProvider::new(k, def.base_url.clone()))
                    .map(|p| Arc::new(p) as Arc<dyn ModelProvider>),
                _ => OpenAiCompatProvider::new(def.base_url.clone(), key, "openai-compatible")
                    .map(|p| Arc::new(p) as Arc<dyn ModelProvider>),
            };
            if let Some(p) = provider {
                // A declared provider named `ollama` replaces the built-in one.
                prefixed.retain(|(n, _)| n != name);
                prefixed.push((name.clone(), p));
            }
        }
        prefixed.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));

        Self {
            aliases,
            prefixed,
            anthropic,
            openai,
            fake,
            pricing: cfg.pricing.clone(),
        }
    }

    /// Which providers are live, for the startup banner and `/_webcortex/models`.
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "aliases": self.aliases,
            "anthropic": self.anthropic.is_some(),
            "openai": self.openai.is_some(),
            "fake": self.fake.is_some(),
            "prefixes": self.prefixed.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
            "priced_models": self.pricing.keys().collect::<Vec<_>>(),
        })
    }

    /// Follow aliases to a concrete model name. Bounded so `a → b → a` cannot
    /// spin.
    pub fn resolve_alias(&self, name: &str) -> String {
        let mut current = name.to_string();
        for _ in 0..8 {
            match self.aliases.get(&current) {
                Some(next) if next != &current => current = next.clone(),
                _ => break,
            }
        }
        current
    }

    /// Pick the provider for a concrete model name and return the name the
    /// provider should be sent.
    pub fn route(&self, concrete: &str) -> Result<(Arc<dyn ModelProvider>, String), String> {
        if let Some(f) = &self.fake {
            return Ok((f.clone(), concrete.to_string()));
        }
        if let Some((prefix, rest)) = concrete.split_once('/') {
            if let Some((_, p)) = self.prefixed.iter().find(|(n, _)| n == prefix) {
                return Ok((p.clone(), rest.to_string()));
            }
            return match prefix {
                "anthropic" => self
                    .anthropic
                    .clone()
                    .map(|p| (p, rest.to_string()))
                    .ok_or_else(|| "ANTHROPIC_API_KEY is not set".to_string()),
                "openai" => self
                    .openai
                    .clone()
                    .map(|p| (p, rest.to_string()))
                    .ok_or_else(|| "OPENAI_API_KEY is not set".to_string()),
                other => Err(format!(
                    "model {concrete:?} names provider {other:?}, which is not declared; \
                     declare it with app.provider({other:?}, base_url=...) or use ollama/, \
                     openai/ or anthropic/"
                )),
            };
        }
        if concrete.starts_with("gpt-") || concrete.starts_with("o1") || concrete.starts_with("o3") || concrete.starts_with("o4") {
            return self
                .openai
                .clone()
                .map(|p| (p, concrete.to_string()))
                .ok_or_else(|| format!("model {concrete:?} needs OPENAI_API_KEY, which is not set"));
        }
        self.anthropic
            .clone()
            .map(|p| (p, concrete.to_string()))
            .ok_or_else(|| {
                format!(
                    "model {concrete:?} needs ANTHROPIC_API_KEY, which is not set. \
                     For a local model use a prefixed name such as ollama/<model>"
                )
            })
    }

    pub fn price_for(&self, model: &str) -> Option<Price> {
        // Try the exact name, then without a provider prefix.
        self.pricing
            .get(model)
            .or_else(|| model.split_once('/').and_then(|(_, m)| self.pricing.get(m)))
            .copied()
    }
}

impl ModelProvider for ProviderRegistry {
    fn name(&self) -> &'static str {
        "registry"
    }

    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest<'a>,
    ) -> BoxFuture<'a, Result<ProviderResponse, String>> {
        Box::pin(async move {
            let concrete = self.resolve_alias(&req.agent.model);
            let (provider, wire_name) = self.route(&concrete)?;
            // The provider must see the stripped name; everything else about
            // the request is unchanged.
            let def = AgentDef { model: wire_name, ..req.agent.clone() };
            let inner = CompletionRequest {
                agent: &def,
                conversation: req.conversation,
                tools: req.tools,
                force_tool: req.force_tool,
                system_suffix: req.system_suffix,
            };
            let mut res = provider.complete(&inner).await?;
            // Report the name the application used (after aliasing, with its
            // prefix) so the ledger groups spend the way the app thinks.
            res.model = concrete;
            Ok(res)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ProviderDef;

    fn cfg() -> ModelsConfig {
        let mut c = ModelsConfig::default();
        c.aliases.insert("fast".into(), "ollama/qwen3.5:9b".into());
        c.aliases.insert("local".into(), "fast".into());
        c.providers.insert(
            "groq".into(),
            ProviderDef { kind: "openai".into(), base_url: "https://x.test/v1".into(), api_key_env: None },
        );
        c.pricing.insert("claude-opus-5".into(), Price { input_per_mtok: 1.0, output_per_mtok: 2.0, ..Default::default() });
        c
    }

    #[test]
    fn aliases_chain_and_user_aliases_override_builtins() {
        let r = ProviderRegistry::from_env(&cfg());
        assert_eq!(r.resolve_alias("local"), "ollama/qwen3.5:9b");
        assert_eq!(r.resolve_alias("default"), BUILTIN_DEFAULT);
        assert_eq!(r.resolve_alias("claude-opus-5"), "claude-opus-5");
    }

    #[test]
    fn a_cyclic_alias_terminates() {
        let mut c = ModelsConfig::default();
        c.aliases.insert("a".into(), "b".into());
        c.aliases.insert("b".into(), "a".into());
        let r = ProviderRegistry::from_env(&c);
        let _ = r.resolve_alias("a");
    }

    #[test]
    fn prefixes_route_to_the_right_provider_and_are_stripped() {
        let r = ProviderRegistry::from_env(&cfg());
        let (p, name) = r.route("ollama/qwen3.5:9b").expect("ollama is built in");
        assert_eq!(p.name(), "ollama");
        assert_eq!(name, "qwen3.5:9b");
        let (p, name) = r.route("groq/llama").expect("declared provider");
        assert_eq!(p.name(), "openai-compatible");
        assert_eq!(name, "llama");
    }

    #[test]
    fn an_unknown_prefix_is_a_clear_error() {
        let r = ProviderRegistry::from_env(&cfg());
        let err = match r.route("nope/model") {
            Err(e) => e,
            Ok(_) => panic!("an unknown prefix must not resolve"),
        };
        assert!(err.contains("not declared"), "{err}");
    }

    #[test]
    fn pricing_ignores_the_provider_prefix() {
        let r = ProviderRegistry::from_env(&cfg());
        assert!(r.price_for("anthropic/claude-opus-5").is_some());
        assert!(r.price_for("ollama/qwen3.5:9b").is_none());
    }
}
