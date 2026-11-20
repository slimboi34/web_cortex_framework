//! A small JSON Schema checker for structured agent output.
//!
//! An agent declared with an output schema finishes by calling a
//! `final_answer` tool, and the runtime checks what it sent before calling the
//! run complete. Providers *ask* a model to follow a tool's schema; only some
//! enforce it, and a local model enforces nothing. Checking here is what makes
//! "the answer has this shape" a property of the run rather than of the prompt.
//!
//! This is deliberately a subset, not a JSON Schema implementation: the
//! keywords WebCortex's own `schema.py` emits, plus the ones people write by
//! hand for a tool. Supported: `type` (a name or a list), `nullable`, `enum`,
//! `const`, `properties`, `required`, `additionalProperties` (boolean or
//! schema), `items`, `prefixItems`, `minItems`, `maxItems`, `minimum`,
//! `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `minLength`,
//! `maxLength`, `anyOf`, `oneOf`, `allOf`. Anything else (`pattern`, `format`,
//! `$ref`) is ignored rather than refused, so an unknown keyword never fails
//! an answer that is otherwise right.

use serde_json::Value;

/// Stop collecting after this many problems: the list is shown to a model,
/// which needs the first few, not all two hundred.
pub const MAX_PROBLEMS: usize = 8;

/// Check `value` against `schema`. Returns every problem found, each naming
/// the JSON pointer it is at; empty means the value conforms.
///
/// ```
/// use serde_json::json;
/// let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]});
/// assert!(webcortex_core::schema::validate(&schema, &json!({"n": 3})).is_empty());
/// assert_eq!(webcortex_core::schema::validate(&schema, &json!({})), vec!["$: missing required property \"n\""]);
/// ```
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    check(schema, value, "$", &mut problems);
    problems
}

/// Why `schema` cannot be used as an agent's output schema, if it cannot. A
/// tool's input must be a JSON object on both wire formats, so the answer is
/// an object too.
pub fn output_schema_problem(schema: &Value) -> Option<String> {
    let Some(obj) = schema.as_object() else {
        return Some("it must be a JSON Schema object".into());
    };
    match obj.get("type") {
        Some(Value::String(t)) if t == "object" => None,
        Some(other) => Some(format!(
            "its top-level type must be \"object\" (the answer is delivered as a tool call), not {other}"
        )),
        None => Some("it must declare \"type\": \"object\" at the top level".into()),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_type(v: &Value, wanted: &str) -> bool {
    match wanted {
        "number" => v.is_number(),
        // 3.0 is an integer as far as JSON Schema is concerned.
        "integer" => match v {
            Value::Number(n) => n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0 && f.is_finite()),
            _ => false,
        },
        other => type_name(v) == other,
    }
}

fn push(problems: &mut Vec<String>, at: &str, message: String) {
    if problems.len() < MAX_PROBLEMS {
        problems.push(format!("{at}: {message}"));
    }
}

fn check(schema: &Value, value: &Value, at: &str, problems: &mut Vec<String>) {
    if problems.len() >= MAX_PROBLEMS {
        return;
    }
    let Some(s) = schema.as_object() else {
        // `true` and `{}` accept anything; `false` accepts nothing.
        if schema == &Value::Bool(false) {
            push(problems, at, "no value is allowed here".into());
        }
        return;
    };

    if value.is_null() && s.get("nullable") == Some(&Value::Bool(true)) {
        return;
    }

    match s.get("type") {
        Some(Value::String(t)) if !is_type(value, t) => {
            push(problems, at, format!("expected {t}, got {}", type_name(value)));
            return;
        }
        Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(|t| is_type(value, t)) => {
            let names: Vec<&str> = ts.iter().filter_map(Value::as_str).collect();
            push(problems, at, format!("expected one of {}, got {}", names.join(" | "), type_name(value)));
            return;
        }
        _ => {}
    }

    if let Some(Value::Array(allowed)) = s.get("enum")
        && !allowed.iter().any(|a| same(a, value))
    {
        let shown: Vec<String> = allowed.iter().take(12).map(Value::to_string).collect();
        push(problems, at, format!("must be one of {}", shown.join(", ")));
    }
    if let Some(c) = s.get("const")
        && !same(c, value)
    {
        push(problems, at, format!("must be {c}"));
    }

    match value {
        Value::Object(map) => {
            if let Some(Value::Array(required)) = s.get("required") {
                for name in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(name) {
                        push(problems, at, format!("missing required property {name:?}"));
                    }
                }
            }
            let props = s.get("properties").and_then(Value::as_object);
            for (key, item) in map {
                let here = format!("{at}.{key}");
                match props.and_then(|p| p.get(key)) {
                    Some(sub) => check(sub, item, &here, problems),
                    None => match s.get("additionalProperties") {
                        Some(Value::Bool(false)) => push(problems, at, format!("unexpected property {key:?}")),
                        Some(sub @ Value::Object(_)) => check(sub, item, &here, problems),
                        _ => {}
                    },
                }
            }
        }
        Value::Array(items) => {
            let n = items.len() as u64;
            if let Some(min) = s.get("minItems").and_then(Value::as_u64)
                && n < min
            {
                push(problems, at, format!("needs at least {min} item(s), has {n}"));
            }
            if let Some(max) = s.get("maxItems").and_then(Value::as_u64)
                && n > max
            {
                push(problems, at, format!("allows at most {max} item(s), has {n}"));
            }
            let prefix = s.get("prefixItems").and_then(Value::as_array);
            for (i, item) in items.iter().enumerate() {
                let here = format!("{at}[{i}]");
                match prefix.and_then(|p| p.get(i)) {
                    Some(sub) => check(sub, item, &here, problems),
                    None => {
                        if let Some(sub) = s.get("items") {
                            check(sub, item, &here, problems);
                        }
                    }
                }
            }
        }
        Value::Number(num) => {
            if let Some(f) = num.as_f64() {
                let bound = |key: &str| s.get(key).and_then(Value::as_f64);
                if let Some(min) = bound("minimum")
                    && f < min
                {
                    push(problems, at, format!("must be >= {min}"));
                }
                if let Some(max) = bound("maximum")
                    && f > max
                {
                    push(problems, at, format!("must be <= {max}"));
                }
                if let Some(min) = bound("exclusiveMinimum")
                    && f <= min
                {
                    push(problems, at, format!("must be > {min}"));
                }
                if let Some(max) = bound("exclusiveMaximum")
                    && f >= max
                {
                    push(problems, at, format!("must be < {max}"));
                }
            }
        }
        Value::String(text) => {
            let n = text.chars().count() as u64;
            if let Some(min) = s.get("minLength").and_then(Value::as_u64)
                && n < min
            {
                push(problems, at, format!("must be at least {min} character(s) long"));
            }
            if let Some(max) = s.get("maxLength").and_then(Value::as_u64)
                && n > max
            {
                push(problems, at, format!("must be at most {max} character(s) long"));
            }
        }
        _ => {}
    }

    if let Some(Value::Array(all)) = s.get("allOf") {
        for sub in all {
            check(sub, value, at, problems);
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(options)) = s.get(key) {
            let matching = options.iter().filter(|sub| validate(sub, value).is_empty()).count();
            if matching == 0 {
                push(problems, at, format!("matches none of the {} alternatives in {key}", options.len()));
            } else if key == "oneOf" && matching > 1 {
                push(problems, at, format!("matches {matching} alternatives in oneOf; exactly one is allowed"));
            }
        }
    }
}

/// Equality as JSON Schema means it: `1` and `1.0` are the same number.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn weather() -> Value {
        json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "minLength": 1},
                "temp_c": {"type": "number", "minimum": -90, "maximum": 60},
                "sky": {"enum": ["clear", "cloud", "rain"]},
                "hours": {"type": "array", "items": {"type": "integer"}, "maxItems": 3},
                "note": {"type": "string", "nullable": true},
            },
            "required": ["city", "temp_c"],
            "additionalProperties": false,
        })
    }

    #[test]
    fn a_conforming_value_has_no_problems() {
        let v = json!({"city": "Paris", "temp_c": 21.5, "sky": "clear", "hours": [9, 12.0], "note": null});
        assert_eq!(validate(&weather(), &v), Vec::<String>::new());
    }

    #[test]
    fn each_problem_names_where_it_is() {
        let v = json!({"city": "", "temp_c": "warm", "sky": "fog", "hours": [1, "two", 3, 4], "extra": 1});
        let problems = validate(&weather(), &v);
        assert!(problems.contains(&"$.city: must be at least 1 character(s) long".to_string()), "{problems:?}");
        assert!(problems.contains(&"$.temp_c: expected number, got string".to_string()), "{problems:?}");
        assert!(problems.iter().any(|p| p.starts_with("$.sky: must be one of")), "{problems:?}");
        assert!(problems.contains(&"$.hours: allows at most 3 item(s), has 4".to_string()), "{problems:?}");
        assert!(problems.contains(&"$.hours[1]: expected integer, got string".to_string()), "{problems:?}");
        assert!(problems.contains(&"$: unexpected property \"extra\"".to_string()), "{problems:?}");
    }

    #[test]
    fn required_properties_and_bounds_are_checked() {
        assert_eq!(validate(&weather(), &json!({"city": "Oslo"})), vec!["$: missing required property \"temp_c\""]);
        assert_eq!(validate(&weather(), &json!({"city": "Oslo", "temp_c": 99})), vec!["$.temp_c: must be <= 60"]);
        assert_eq!(validate(&weather(), &json!("Oslo")), vec!["$: expected object, got string"]);
    }

    #[test]
    fn alternatives_type_lists_and_const_work() {
        let s = json!({"anyOf": [{"type": "string"}, {"type": "integer", "minimum": 0}]});
        assert!(validate(&s, &json!("x")).is_empty());
        assert!(validate(&s, &json!(3)).is_empty());
        assert_eq!(validate(&s, &json!(-1)).len(), 1);
        let s = json!({"oneOf": [{"type": "number"}, {"type": "integer"}]});
        assert_eq!(validate(&s, &json!(2)).len(), 1);
        assert!(validate(&s, &json!(2.5)).is_empty());
        let s = json!({"type": ["string", "null"]});
        assert!(validate(&s, &json!(null)).is_empty());
        assert_eq!(validate(&s, &json!(1)), vec!["$: expected one of string | null, got integer"]);
        assert!(validate(&json!({"const": 1}), &json!(1.0)).is_empty());
        assert_eq!(validate(&json!({"allOf": [{"minimum": 2}, {"maximum": 1}]}), &json!(1)), vec!["$: must be >= 2"]);
        assert_eq!(validate(&json!(false), &json!(1)), vec!["$: no value is allowed here"]);
    }

    #[test]
    fn unknown_keywords_are_ignored_and_the_list_is_capped() {
        assert!(validate(&json!({"type": "string", "pattern": "^a", "format": "email"}), &json!("zzz")).is_empty());
        let s = json!({"type": "array", "items": {"type": "integer"}});
        let many: Vec<Value> = (0..50).map(|_| json!("x")).collect();
        assert_eq!(validate(&s, &Value::Array(many)).len(), MAX_PROBLEMS);
    }

    #[test]
    fn an_output_schema_must_be_an_object_schema() {
        assert!(output_schema_problem(&weather()).is_none());
        assert!(output_schema_problem(&json!({"type": "array"})).unwrap().contains("object"));
        assert!(output_schema_problem(&json!({})).is_some());
        assert!(output_schema_problem(&json!("string")).is_some());
    }
}
