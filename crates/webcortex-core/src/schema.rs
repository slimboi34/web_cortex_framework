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
//! schema), `items`, `prefixItems`, `minItems`, `maxItems`, `uniqueItems`,
//! `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `minLength`,
//! `maxLength`, `anyOf`, `oneOf`, `allOf`, and `$ref` to a definition under
//! the root's `$defs` (`"#/$defs/Node"`, which is how a recursive type is
//! written). Anything else (`pattern`, `format`, `patternProperties`, any
//! other `$ref`) is ignored rather than refused, so an unknown keyword never
//! fails an answer that is otherwise right. Because `patternProperties` is not
//! evaluated, `additionalProperties` is not applied in a schema that has it:
//! which keys are "additional" depends on the patterns.
//!
//! Numbers compare as JSON Schema means them: `1` equals `1.0` at any depth,
//! and two integers compare exactly, however large, instead of through `f64`.
//! Count keywords (`minItems`, `maxLength`, …) accept an integral float such as
//! `2.0`, which JSON Schema counts as an integer.

use serde_json::{Map, Number, Value};
use std::cmp::Ordering;

/// Stop collecting after this many problems: the list is shown to a model,
/// which needs the first few, not all two hundred.
pub const MAX_PROBLEMS: usize = 8;

/// How many `$ref`s may be followed at one place in a value before the
/// checker stops: a chain this long that consumes none of the value is a
/// mistake, and a cycle would otherwise never end.
const MAX_REFS_IN_PLACE: usize = 32;

/// The JSON Schema type names.
const TYPE_NAMES: [&str; 7] = ["null", "boolean", "integer", "number", "string", "array", "object"];

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
    Checker { root: schema }.check(schema, value, "$", &[], &mut problems);
    problems
}

/// Why `schema` cannot be used as an agent's output schema, if it cannot. A
/// tool's input must be a JSON object on both wire formats, so the answer is
/// an object too. A keyword of the wrong type (`"required": "name"`, a
/// negative `maxItems`, a `$ref` to a definition that does not exist) is
/// refused here, at boot, rather than silently ignored on every answer.
pub fn output_schema_problem(schema: &Value) -> Option<String> {
    let Some(obj) = schema.as_object() else {
        return Some("it must be a JSON Schema object".into());
    };
    match obj.get("type") {
        Some(Value::String(t)) if t == "object" => {}
        Some(other) => {
            return Some(format!(
                "its top-level type must be \"object\" (the answer is delivered as a tool call), not {other}"
            ));
        }
        None => return Some("it must declare \"type\": \"object\" at the top level".into()),
    }
    malformed(schema, schema, "#").and_then(|()| ref_cycle(schema)).err()
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

/// The definition a `$ref` names, when it is a local reference into the root
/// schema's `$defs` (`#/$defs/Name`, percent- and pointer-escaped as usual).
/// Any other reference is not followed.
fn resolve_local<'s>(root: &'s Value, reference: &str) -> Option<&'s Value> {
    let fragment = reference.strip_prefix('#')?;
    let pointer = percent_encoding::percent_decode_str(fragment).decode_utf8().ok()?;
    if !pointer.starts_with("/$defs/") {
        return None;
    }
    root.pointer(&pointer)
}

/// A count keyword (`minItems`, `maxLength`, …): a non-negative integer,
/// written either as one or as an integral float such as `2.0`.
fn count_keyword(s: &Map<String, Value>, key: &str) -> Option<u64> {
    count_value(s.get(key)?)
}

fn count_value(v: &Value) -> Option<u64> {
    let n = v.as_number()?;
    n.as_u64().or_else(|| {
        let f = n.as_f64()?;
        // `as` saturates, so 1e300 counts as u64::MAX.
        (f.is_finite() && f >= 0.0 && f.fract() == 0.0).then_some(f as u64)
    })
}

struct Checker<'s> {
    root: &'s Value,
}

impl<'s> Checker<'s> {
    /// `refs` are the references followed at this place in the value so far:
    /// it is emptied whenever the check descends into a property or an item,
    /// so a recursive type may recurse as deep as the value does, but a
    /// reference that comes back to itself without consuming anything stops.
    fn check(&self, schema: &'s Value, value: &Value, at: &str, refs: &[&'s str], problems: &mut Vec<String>) {
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

        if let Some(Value::String(reference)) = s.get("$ref")
            && let Some(target) = resolve_local(self.root, reference)
        {
            if refs.contains(&reference.as_str()) {
                push(problems, at, format!("the schema's $ref {reference:?} refers back to itself here"));
            } else if refs.len() >= MAX_REFS_IN_PLACE {
                push(problems, at, format!("the schema follows more than {MAX_REFS_IN_PLACE} $refs here"));
            } else {
                let mut chain = refs.to_vec();
                chain.push(reference.as_str());
                self.check(target, value, at, &chain, problems);
            }
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
                // Which keys `additionalProperties` covers depends on the
                // patterns, which are not evaluated; applying it anyway would
                // refuse keys the patterns allow.
                let patterned = s.contains_key("patternProperties");
                for (key, item) in map {
                    let here = format!("{at}.{key}");
                    match props.and_then(|p| p.get(key)) {
                        Some(sub) => self.check(sub, item, &here, &[], problems),
                        None if patterned => {}
                        None => match s.get("additionalProperties") {
                            Some(Value::Bool(false)) => push(problems, at, format!("unexpected property {key:?}")),
                            Some(sub @ Value::Object(_)) => self.check(sub, item, &here, &[], problems),
                            _ => {}
                        },
                    }
                }
            }
            Value::Array(items) => {
                let n = items.len() as u64;
                if let Some(min) = count_keyword(s, "minItems")
                    && n < min
                {
                    push(problems, at, format!("needs at least {min} item(s), has {n}"));
                }
                if let Some(max) = count_keyword(s, "maxItems")
                    && n > max
                {
                    push(problems, at, format!("allows at most {max} item(s), has {n}"));
                }
                if s.get("uniqueItems") == Some(&Value::Bool(true))
                    && let Some((first, again)) = first_duplicate(items)
                {
                    push(problems, at, format!("items must be unique, but items {first} and {again} are equal"));
                }
                let prefix = s.get("prefixItems").and_then(Value::as_array);
                for (i, item) in items.iter().enumerate() {
                    let here = format!("{at}[{i}]");
                    match prefix.and_then(|p| p.get(i)) {
                        Some(sub) => self.check(sub, item, &here, &[], problems),
                        None => {
                            if let Some(sub) = s.get("items") {
                                self.check(sub, item, &here, &[], problems);
                            }
                        }
                    }
                }
            }
            Value::Number(num) => {
                let bound = |key: &str| s.get(key).and_then(Value::as_number);
                let below = |b: &Number| num_cmp(num, b) == Some(Ordering::Less);
                let above = |b: &Number| num_cmp(num, b) == Some(Ordering::Greater);
                if let Some(min) = bound("minimum")
                    && below(min)
                {
                    push(problems, at, format!("must be >= {min}"));
                }
                if let Some(max) = bound("maximum")
                    && above(max)
                {
                    push(problems, at, format!("must be <= {max}"));
                }
                if let Some(min) = bound("exclusiveMinimum")
                    && !above(min)
                {
                    push(problems, at, format!("must be > {min}"));
                }
                if let Some(max) = bound("exclusiveMaximum")
                    && !below(max)
                {
                    push(problems, at, format!("must be < {max}"));
                }
            }
            Value::String(text) => {
                let n = text.chars().count() as u64;
                if let Some(min) = count_keyword(s, "minLength")
                    && n < min
                {
                    push(problems, at, format!("must be at least {min} character(s) long"));
                }
                if let Some(max) = count_keyword(s, "maxLength")
                    && n > max
                {
                    push(problems, at, format!("must be at most {max} character(s) long"));
                }
            }
            _ => {}
        }

        if let Some(Value::Array(all)) = s.get("allOf") {
            for sub in all {
                self.check(sub, value, at, refs, problems);
            }
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(Value::Array(options)) = s.get(key) {
                let matching = options
                    .iter()
                    .filter(|sub| {
                        let mut local = Vec::new();
                        self.check(sub, value, at, refs, &mut local);
                        local.is_empty()
                    })
                    .count();
                if matching == 0 {
                    push(problems, at, format!("matches none of the {} alternatives in {key}", options.len()));
                } else if key == "oneOf" && matching > 1 {
                    push(problems, at, format!("matches {matching} alternatives in oneOf; exactly one is allowed"));
                }
            }
        }
    }
}

/// An integer's exact value. `None` for a float.
fn exact_int(n: &Number) -> Option<i128> {
    n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from))
}

/// An integer against a float, exactly: no rounding of the integer to `f64`.
fn cmp_int_float(x: i128, f: f64) -> Option<Ordering> {
    // 2^127: every integral f64 below it converts to i128 exactly.
    const LIMIT: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;
    if f.is_nan() {
        return None;
    }
    if f >= LIMIT {
        return Some(Ordering::Less);
    }
    if f < -LIMIT {
        return Some(Ordering::Greater);
    }
    let whole = f.trunc();
    Some(x.cmp(&(whole as i128)).then(if f > whole {
        Ordering::Less
    } else if f < whole {
        Ordering::Greater
    } else {
        Ordering::Equal
    }))
}

/// Compare two JSON numbers. Two integers compare exactly as `i128` (2^53 + 1
/// is not 2^53); an integer against a float compares without rounding the
/// integer; two floats compare as `f64`.
fn num_cmp(a: &Number, b: &Number) -> Option<Ordering> {
    match (exact_int(a), exact_int(b)) {
        (Some(x), Some(y)) => Some(x.cmp(&y)),
        (Some(x), None) => cmp_int_float(x, b.as_f64()?),
        (None, Some(y)) => cmp_int_float(y, a.as_f64()?).map(Ordering::reverse),
        (None, None) => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

/// Equality as JSON Schema means it, at any depth: `1` and `1.0` are the
/// same number, in an array or an object as much as on their own.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => num_cmp(x, y) == Some(Ordering::Equal),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// The indices of the first two equal items, by [`same`]'s equality. Each
/// item is reduced to a canonical text in which equal numbers are spelled
/// alike, so this is one pass rather than a comparison of every pair.
fn first_duplicate(items: &[Value]) -> Option<(usize, usize)> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, item) in items.iter().enumerate() {
        let mut key = String::new();
        canonical(item, &mut key);
        if let Some(&first) = seen.get(&key) {
            return Some((first, i));
        }
        seen.insert(key, i);
    }
    None
}

fn canonical(v: &Value, out: &mut String) {
    use std::fmt::Write;
    // As in `cmp_int_float`: integral floats below 2^127 are spelled as the
    // integer they equal.
    const LIMIT: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;
    match v {
        Value::Number(n) => match (exact_int(n), n.as_f64()) {
            (Some(i), _) => {
                let _ = write!(out, "{i}");
            }
            (None, Some(f)) if f.fract() == 0.0 && f.abs() < LIMIT => {
                let _ = write!(out, "{}", f as i128);
            }
            (None, f) => {
                let _ = write!(out, "f{:?}", f.unwrap_or(f64::NAN));
            }
        },
        Value::Array(items) => {
            out.push('[');
            for item in items {
                canonical(item, out);
                out.push(',');
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for k in keys {
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(&map[k], out);
                out.push(',');
            }
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// A JSON pointer segment for `key`.
fn segment(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The first keyword in `schema` whose value has the wrong type, by its
/// pointer. Unknown keywords are not looked at.
fn malformed(root: &Value, schema: &Value, at: &str) -> Result<(), String> {
    let s = match schema {
        Value::Object(s) => s,
        Value::Bool(_) => return Ok(()),
        other => return Err(format!("{at} must be a schema (an object, or true or false), not {other}")),
    };
    let here = |key: &str| format!("{at}/{}", segment(key));

    match s.get("type") {
        None => {}
        Some(Value::String(name)) => known_type(name, &here("type"))?,
        Some(Value::Array(names)) if !names.is_empty() => {
            for (i, name) in names.iter().enumerate() {
                match name {
                    Value::String(name) => known_type(name, &format!("{}/{i}", here("type")))?,
                    other => return Err(format!("{}/{i} must be a type name, not {other}", here("type"))),
                }
            }
        }
        Some(other) => return Err(format!("{} must be a type name or a non-empty list of them, not {other}", here("type"))),
    }
    match s.get("properties") {
        None => {}
        Some(Value::Object(props)) => {
            for (name, sub) in props {
                malformed(root, sub, &format!("{}/{}", here("properties"), segment(name)))?;
            }
        }
        Some(other) => return Err(format!("{} must be an object of property schemas, not {other}", here("properties"))),
    }
    match s.get("required") {
        None => {}
        Some(Value::Array(names)) if names.iter().all(Value::is_string) => {}
        Some(other) => return Err(format!("{} must be a list of property names, not {other}", here("required"))),
    }
    if let Some(other) = s.get("enum").filter(|e| !e.is_array()) {
        return Err(format!("{} must be a list of the allowed values, not {other}", here("enum")));
    }
    for key in ["minItems", "maxItems", "minLength", "maxLength"] {
        if let Some(v) = s.get(key)
            && count_value(v).is_none()
        {
            return Err(format!("{} must be a non-negative integer, not {v}", here(key)));
        }
    }
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        match s.get(key) {
            None | Some(Value::Number(_)) => {}
            Some(Value::Bool(_)) if key.starts_with("exclusive") => {
                return Err(format!(
                    "{} must be a number (the bound itself, as in JSON Schema 2020-12); the true/false form is \
                     from draft 4 and OpenAPI 3.0",
                    here(key)
                ));
            }
            Some(other) => return Err(format!("{} must be a number, not {other}", here(key))),
        }
    }
    for key in ["nullable", "uniqueItems"] {
        if let Some(v) = s.get(key).filter(|v| !v.is_boolean()) {
            return Err(format!("{} must be true or false, not {v}", here(key)));
        }
    }
    if let Some(sub) = s.get("additionalProperties") {
        malformed(root, sub, &here("additionalProperties"))?;
    }
    match s.get("items") {
        None => {}
        Some(Value::Array(_)) => {
            return Err(format!(
                "{} must be one schema for every item; for a fixed list of positions use prefixItems",
                here("items")
            ));
        }
        Some(sub) => malformed(root, sub, &here("items"))?,
    }
    match s.get("prefixItems") {
        None => {}
        Some(Value::Array(subs)) => {
            for (i, sub) in subs.iter().enumerate() {
                malformed(root, sub, &format!("{}/{i}", here("prefixItems")))?;
            }
        }
        Some(other) => return Err(format!("{} must be a list of schemas, not {other}", here("prefixItems"))),
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        match s.get(key) {
            None => {}
            Some(Value::Array(subs)) if !subs.is_empty() => {
                for (i, sub) in subs.iter().enumerate() {
                    malformed(root, sub, &format!("{}/{i}", here(key)))?;
                }
            }
            Some(other) => return Err(format!("{} must be a non-empty list of schemas, not {other}", here(key))),
        }
    }
    match s.get("$defs") {
        None => {}
        Some(Value::Object(defs)) => {
            for (name, sub) in defs {
                malformed(root, sub, &format!("{}/{}", here("$defs"), segment(name)))?;
            }
        }
        Some(other) => return Err(format!("{} must be an object of named schemas, not {other}", here("$defs"))),
    }
    match s.get("$ref") {
        None => {}
        Some(Value::String(reference)) => {
            let local = reference
                .strip_prefix('#')
                .and_then(|f| percent_encoding::percent_decode_str(f).decode_utf8().ok())
                .is_some_and(|p| p.starts_with("/$defs/"));
            if local && resolve_local(root, reference).is_none() {
                return Err(format!(
                    "{} is {reference:?}, which names no definition under the schema's top-level $defs",
                    here("$ref")
                ));
            }
        }
        Some(other) => return Err(format!("{} must be a string, not {other}", here("$ref"))),
    }
    Ok(())
}

fn known_type(name: &str, at: &str) -> Result<(), String> {
    if TYPE_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(format!("{at} is {name:?}, which is not a JSON Schema type; use one of {}", TYPE_NAMES.join(", ")))
    }
}

/// A definition that reaches itself through `$ref`s (directly, or through
/// `allOf`/`anyOf`/`oneOf`) without describing any part of the value — so
/// checking it would never get anywhere. Recursion through a property or an
/// item is fine: each step consumes part of the value.
fn ref_cycle(root: &Value) -> Result<(), String> {
    let Some(defs) = root.get("$defs").and_then(Value::as_object) else { return Ok(()) };
    for (name, start) in defs {
        let mut stack = in_place_refs(start);
        let mut seen: Vec<*const Value> = Vec::new();
        while let Some(reference) = stack.pop() {
            let Some(target) = resolve_local(root, reference) else { continue };
            if std::ptr::eq(target, start) {
                return Err(format!(
                    "#/$defs/{} refers back to itself through {reference:?} without describing any part of the \
                     value, so no answer could ever be checked against it",
                    segment(name)
                ));
            }
            if !seen.contains(&(target as *const Value)) {
                seen.push(target);
                stack.extend(in_place_refs(target));
            }
        }
    }
    Ok(())
}

/// The `$ref`s that apply at the same place in the value as `schema` itself.
fn in_place_refs(schema: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    if let Some(s) = schema.as_object() {
        if let Some(Value::String(r)) = s.get("$ref") {
            out.push(r.as_str());
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            for sub in s.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
                out.extend(in_place_refs(sub));
            }
        }
    }
    out
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

    fn valid(schema: Value, value: Value) -> bool {
        validate(&schema, &value).is_empty()
    }

    #[test]
    fn count_keywords_written_as_integral_floats_are_enforced() {
        assert!(!valid(json!({"type": "array", "minItems": 2.0}), json!([1])));
        assert!(!valid(json!({"type": "array", "maxItems": 1.0}), json!([1, 2, 3])));
        assert!(!valid(json!({"type": "string", "maxLength": 2.0}), json!("abcdef")));
        assert!(!valid(json!({"type": "string", "minLength": 3.0}), json!("")));
        assert!(valid(json!({"type": "string", "minLength": 3.0}), json!("abc")));
        assert_eq!(validate(&json!({"maxItems": 1.0}), &json!([1, 2])), vec!["$: allows at most 1 item(s), has 2"]);
    }

    #[test]
    fn enum_and_const_compare_numbers_as_numbers_at_any_depth() {
        assert!(valid(json!({"const": [1]}), json!([1.0])));
        assert!(valid(json!({"const": {"k": 1}}), json!({"k": 1.0})));
        assert!(valid(json!({"enum": [[1, 2], "x"]}), json!([1.0, 2])));
        assert!(valid(json!({"enum": [{"a": 2.0}]}), json!({"a": 2})));
        assert!(!valid(json!({"const": {"k": 1}}), json!({"k": 1, "extra": 2})));
        assert!(!valid(json!({"const": [1, 2]}), json!([1])));
        assert!(!valid(json!({"const": {"k": 1}}), json!({"k": "1"})));
    }

    #[test]
    fn big_integers_compare_exactly() {
        let two53 = 9_007_199_254_740_992u64;
        assert!(valid(json!({"type": "integer", "maximum": two53}), json!(two53)));
        assert!(!valid(json!({"type": "integer", "maximum": two53}), json!(two53 + 1)));
        assert!(!valid(json!({"exclusiveMaximum": two53 + 1}), json!(two53 + 1)));
        assert!(valid(json!({"exclusiveMaximum": two53 + 1}), json!(two53)));
        assert!(!valid(json!({"minimum": 9_223_372_036_854_775_808u64}), json!(9_223_372_036_854_775_807i64)));
        assert!(!valid(json!({"exclusiveMinimum": -9_007_199_254_740_993i64}), json!(-9_007_199_254_740_993i64)));
        assert!(!valid(json!({"enum": [two53 + 1]}), json!(two53)));
        assert!(!valid(json!({"const": 18_014_398_509_481_985u64}), json!(18_014_398_509_481_984u64)));
        // Mixed integer and float: exact, not through f64.
        assert!(!valid(json!({"const": two53 + 1}), json!(9_007_199_254_740_992.0)));
        assert!(valid(json!({"const": two53}), json!(9_007_199_254_740_992.0)));
        assert!(valid(json!({"minimum": 2}), json!(2.5)));
        assert!(!valid(json!({"maximum": 2}), json!(2.5)));
        assert!(!valid(json!({"minimum": -2}), json!(-2.5)));
        assert!(valid(json!({"maximum": 2.5}), json!(2)));
        assert!(valid(json!({"maximum": 1e300}), json!(u64::MAX)));
        assert!(!valid(json!({"minimum": 1e300}), json!(u64::MAX)));
    }

    #[test]
    fn unique_items_is_checked_with_numeric_equality() {
        let set = json!({"type": "array", "items": {"type": "string"}, "uniqueItems": true});
        assert_eq!(validate(&set, &json!(["urgent", "urgent"])), vec!["$: items must be unique, but items 0 and 1 are equal"]);
        assert!(valid(set.clone(), json!(["urgent", "later"])));
        assert!(valid(set, json!([])));
        let any = json!({"uniqueItems": true});
        assert!(!valid(any.clone(), json!([1, 2, 1.0])));
        assert!(!valid(any.clone(), json!([{"a": [1]}, {"a": [1.0]}])));
        assert!(valid(any.clone(), json!([1, "1", [1], {"1": 1}, true, null, 1.5])));
        assert!(valid(any.clone(), json!([9_007_199_254_740_993u64, 9_007_199_254_740_992u64])));
        assert!(valid(json!({"uniqueItems": false}), json!([1, 1])));
    }

    #[test]
    fn pattern_properties_switch_off_additional_properties_rather_than_refuse() {
        let s = json!({"type": "object", "patternProperties": {"^x-": {"type": "string"}}, "additionalProperties": false});
        assert!(valid(s, json!({"x-team": "core"})));
        let s = json!({"type": "object", "properties": {"a": {"type": "integer"}}, "patternProperties": {"^x-": {}},
                       "additionalProperties": false});
        assert_eq!(validate(&s, &json!({"a": "no", "x-b": 1})), vec!["$.a: expected integer, got string"]);
    }

    fn tree() -> Value {
        json!({
            "type": "object",
            "properties": {"root": {"$ref": "#/$defs/Node"}},
            "required": ["root"],
            "$defs": {"Node": {
                "type": "object",
                "properties": {
                    "value": {"type": "integer"},
                    "children": {"type": "array", "items": {"$ref": "#/$defs/Node"}},
                    "parent": {"$ref": "#/$defs/Node", "nullable": true},
                },
                "required": ["value"],
                "additionalProperties": false,
            }},
        })
    }

    #[test]
    fn local_refs_are_followed_so_a_recursive_type_is_checked_at_every_depth() {
        let good = json!({"root": {"value": 1, "children": [{"value": 2, "children": [{"value": 3, "parent": null}]}]}});
        assert_eq!(validate(&tree(), &good), Vec::<String>::new());
        let bad = json!({"root": {"value": 1, "children": [{"value": 2, "children": [{"value": "three"}]}]}});
        assert_eq!(validate(&tree(), &bad), vec!["$.root.children[0].children[0].value: expected integer, got string"]);
        let extra = json!({"root": {"value": 1, "children": [{"value": 2, "children": [{"value": 3, "x": 1}]}]}});
        assert_eq!(validate(&tree(), &extra), vec!["$.root.children[0].children[0]: unexpected property \"x\""]);
        // Siblings of $ref apply too, as in 2020-12.
        let s = json!({"$defs": {"N": {"type": "integer"}}, "$ref": "#/$defs/N", "minimum": 5});
        assert!(!valid(s.clone(), json!(3)));
        assert!(!valid(s.clone(), json!("x")));
        assert!(valid(s, json!(7)));
        // Other references are still ignored.
        assert!(valid(json!({"$ref": "https://example.com/schema"}), json!(1)));
        assert!(valid(json!({"$ref": "#/definitions/X", "definitions": {"X": {"type": "string"}}}), json!(1)));
        assert!(valid(json!({"$ref": "#/$defs/Missing"}), json!(1)));
    }

    #[test]
    fn a_reference_cycle_that_consumes_nothing_ends() {
        let s = json!({"$defs": {"A": {"$ref": "#/$defs/B"}, "B": {"$ref": "#/$defs/A"}}, "$ref": "#/$defs/A"});
        let problems = validate(&s, &json!(1));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("refers back to itself"), "{problems:?}");
        let s = json!({"$defs": {"A": {"anyOf": [{"$ref": "#/$defs/A"}, {"type": "string"}]}}, "$ref": "#/$defs/A"});
        assert!(valid(s.clone(), json!("x")));
        assert!(!valid(s, json!(1)));
    }

    #[test]
    fn a_malformed_output_schema_is_refused_with_where_and_why() {
        let wrapped = |sub: Value| json!({"type": "object", "properties": {"v": sub}});
        for (bad, why) in [
            (wrapped(json!({"type": "str"})), "#/properties/v/type is \"str\", which is not a JSON Schema type"),
            (wrapped(json!({"type": 3})), "#/properties/v/type must be a type name"),
            (wrapped(json!({"type": []})), "#/properties/v/type must be a type name or a non-empty list"),
            (wrapped(json!({"type": ["string", 1]})), "#/properties/v/type/1 must be a type name"),
            (json!({"type": "object", "properties": []}), "#/properties must be an object"),
            (json!({"type": "object", "required": "name"}), "#/required must be a list of property names"),
            (json!({"type": "object", "required": ["a", 1]}), "#/required must be a list"),
            (wrapped(json!({"enum": "a"})), "#/properties/v/enum must be a list"),
            (wrapped(json!({"maxItems": -1})), "#/properties/v/maxItems must be a non-negative integer, not -1"),
            (wrapped(json!({"minLength": 2.5})), "#/properties/v/minLength must be a non-negative integer, not 2.5"),
            (wrapped(json!({"maxLength": "3"})), "maxLength must be a non-negative integer"),
            (wrapped(json!({"minimum": "0"})), "#/properties/v/minimum must be a number"),
            (wrapped(json!({"exclusiveMinimum": true})), "the true/false form is from draft 4"),
            (wrapped(json!({"uniqueItems": "yes"})), "uniqueItems must be true or false"),
            (wrapped(json!({"items": [{"type": "string"}]})), "use prefixItems"),
            (wrapped(json!({"items": 1})), "#/properties/v/items must be a schema"),
            (wrapped(json!({"anyOf": []})), "anyOf must be a non-empty list of schemas"),
            (wrapped(json!({"anyOf": [{"type": "nope"}]})), "#/properties/v/anyOf/0/type is \"nope\""),
            (wrapped(json!({"additionalProperties": {"type": "x"}})), "additionalProperties/type"),
            (wrapped(json!({"$ref": "#/$defs/Missing"})), "names no definition"),
            (wrapped(json!({"$ref": 5})), "$ref must be a string"),
            (json!({"type": "object", "$defs": {"N": {"minItems": -2}}}), "#/$defs/N/minItems"),
            (json!({"type": "object", "$defs": {"A": {"anyOf": [{"$ref": "#/$defs/A"}, {"type": "null"}]}}}),
             "#/$defs/A refers back to itself"),
        ] {
            let problem = output_schema_problem(&bad).unwrap_or_else(|| panic!("accepted {bad}"));
            assert!(problem.contains(why), "{bad}: {problem}");
        }
        // Well-formed schemas, including what schema.py emits, still pass.
        for good in [
            weather(),
            tree(),
            wrapped(json!({"type": "array", "items": {"type": "string"}, "uniqueItems": true, "minItems": 1.0})),
            wrapped(json!({"type": ["string", "null"], "pattern": "^a", "format": "email", "title": "T"})),
            wrapped(json!({"type": "array", "prefixItems": [{"type": "integer"}, true], "minItems": 2, "maxItems": 2})),
            wrapped(json!({"anyOf": [{"type": "integer"}, {"type": "string"}], "exclusiveMinimum": 0})),
            wrapped(json!({"$ref": "https://example.com/elsewhere"})),
            wrapped(json!({"type": "object", "additionalProperties": true, "patternProperties": {"^x": {}}})),
        ] {
            assert_eq!(output_schema_problem(&good), None, "{good}");
        }
    }
}
