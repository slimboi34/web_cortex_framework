//! Context providers.
//!
//! A run is only as good as what it knows at step one. A context provider is a
//! named, declared source — a constant, a SQL query executed in Rust, or a
//! Python function — that is resolved when a run starts and handed to the
//! model as a delimited block in the system prompt. Declared once, reused by
//! any agent, behaviour or flow that names it, bounded in size because it is
//! re-sent on every step.

use crate::app::App;
use crate::auth::Principal;
use crate::http::WebCortexRequest;
use crate::manifest::{ContextDef, ContextSource};
use serde_json::Value;

/// Resolve several providers and render them as system-prompt blocks.
pub async fn resolve(
    app: &App,
    names: &[String],
    principal: &Principal,
    input: &Value,
) -> Result<String, String> {
    let mut out = String::new();
    for name in names {
        let def = app
            .manifest
            .context(name)
            .ok_or_else(|| format!("context {name:?} is not declared"))?;
        let value = resolve_one(app, def, principal, input).await?;
        let text = clip(&render(&value), def.max_chars);
        if !out.is_empty() {
            out.push('\n');
        }
        if def.description.is_empty() {
            out.push_str(&format!("<context name=\"{name}\">\n{text}\n</context>\n"));
        } else {
            out.push_str(&format!(
                "<context name=\"{name}\" description=\"{}\">\n{text}\n</context>\n",
                def.description.replace('"', "'")
            ));
        }
    }
    Ok(out.trim_end().to_string())
}

/// Resolve one provider to its raw value.
pub async fn resolve_one(
    app: &App,
    def: &ContextDef,
    principal: &Principal,
    input: &Value,
) -> Result<Value, String> {
    match &def.source {
        ContextSource::Static { value } => Ok(value.clone()),
        ContextSource::Query { sql, params, returns } => {
            #[cfg(feature = "sqlite")]
            {
                let db = app
                    .db()
                    .ok_or_else(|| format!("context {:?} runs a query but no database is configured", def.name))?;
                let bindings: Vec<Value> = params
                    .iter()
                    .map(|n| {
                        if n == "@principal" {
                            // Anonymous callers share one id: bind nothing
                            // rather than pool their data.
                            if principal.root_is_anonymous() {
                                Value::Null
                            } else {
                                Value::String(principal.root_id().to_string())
                            }
                        } else {
                            input.get(n).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();
                db.run(sql, &bindings, *returns)
                    .await
                    .map_err(|e| format!("context {:?}: {}", def.name, e.message()))
            }
            #[cfg(not(feature = "sqlite"))]
            {
                let _ = (sql, params, returns, principal, input);
                Err("this build has no database support".into())
            }
        }
        ContextSource::Python { handler } => {
            let mut req = WebCortexRequest::synthetic("POST", &format!("/_context/{}", def.name));
            req.principal = principal.clone();
            req.body = bytes::Bytes::from(serde_json::to_vec(input).map_err(|e| e.to_string())?);
            let res = app.bridge().call(*handler, req).await?;
            if res.status >= 400 {
                return Err(format!(
                    "context {:?} failed with {}: {}",
                    def.name,
                    res.status,
                    String::from_utf8_lossy(&res.body)
                ));
            }
            Ok(res.json_value())
        }
    }
}

pub fn render(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Cut at a character boundary, saying so.
pub fn clip(text: &str, max_chars: usize) -> String {
    if max_chars == 0 || text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}\n…[context truncated to {max_chars} characters]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_is_character_aware_and_marks_the_cut() {
        assert_eq!(clip("abc", 10), "abc");
        let out = clip("ééééé", 2);
        assert!(out.starts_with("éé"));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn strings_render_verbatim_and_objects_render_pretty() {
        assert_eq!(render(&serde_json::json!("hi")), "hi");
        assert!(render(&serde_json::json!({"a": 1})).contains("\"a\": 1"));
    }
}
