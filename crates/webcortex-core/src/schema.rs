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
//! and two integers in the 64-bit range (`-2^63` to `2^64 - 1`) compare
//! exactly instead of through `f64`. Count keywords (`minItems`, `maxLength`,
//! …) accept an integral float such as `2.0`, which JSON Schema counts as an
//! integer.
//!
//! An integer outside that range cannot be carried exactly: the JSON parser
//! keeps it as the nearest `f64` (serde_json without `arbitrary_precision`,
//! which would change how every number in the runtime is held). So the checker
//! never lets such a value pass on a rounding: `type: integer` refuses it, and
//! a `minimum`/`maximum`, `const`, `enum` or `uniqueItems` decision that rests
//! on two values being equal only as doubles, where one is beyond that range,
//! is refused with a problem saying so.
//!
//! Checking takes time polynomial in the size of the schema and the value.
//! Alternatives (`anyOf`, `oneOf`) are tried fail-fast — the first problem
//! ends a try, `anyOf` stops at the first alternative that matches and
//! `oneOf` at the second — and the verdict of a `$ref` target on an object or
//! an array is remembered, so a recursive union (an expression tree, an AST)
//! checks each part of the value once per member rather than once per path to
//! it. A step budget bounds what is left (a schema whose alternatives multiply
//! at one place): past it the check stops with a problem rather than run on.

use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Stop collecting after this many problems: the list is shown to a model,
/// which needs the first few, not all two hundred.
pub const MAX_PROBLEMS: usize = 8;

/// How many `$ref`s may be followed at one place in a value before the
/// checker stops: a chain this long that consumes none of the value is a
/// mistake, and a cycle would otherwise never end. The boot check
/// ([`output_schema_problem`]) refuses a schema that could need more.
pub const MAX_REFS_IN_PLACE: usize = 32;

/// The step budget of one check is this many steps for each JSON value in the
/// checked value (each object, array, string, number, …), never less than
/// [`MIN_STEPS`] and never more than [`MAX_STEPS`]. A step is one schema
/// applied to one value. Ordinary schemas, unions included, take a handful of
/// steps per value, so only a schema whose alternatives multiply at one place
/// comes near it.
pub const STEPS_PER_VALUE: usize = 256;

/// The least step budget a check gets, whatever the size of the value.
pub const MIN_STEPS: usize = 100_000;

/// The most steps any one check may take (a fraction of a second in a release
/// build), however large the value.
pub const MAX_STEPS: usize = 4_000_000;

/// The most verdicts one check remembers. Past it checking goes on without
/// remembering more (the step budget still bounds it), so a huge value cannot
/// turn the memo into a large allocation.
const MAX_MEMO: usize = 1 << 18;

/// The JSON Schema type names.
const TYPE_NAMES: [&str; 7] = ["null", "boolean", "integer", "number", "string", "array", "object"];

/// Check `value` against `schema`. Returns every problem found (up to
/// [`MAX_PROBLEMS`]), each naming the JSON pointer it is at; empty means the
/// value conforms.
///
/// ```
/// use serde_json::json;
/// let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]});
/// assert!(webcortex_core::schema::validate(&schema, &json!({"n": 3})).is_empty());
/// assert_eq!(webcortex_core::schema::validate(&schema, &json!({})), vec!["$: missing required property \"n\""]);
/// ```
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let budget = STEPS_PER_VALUE.saturating_mul(count_values(value)).clamp(MIN_STEPS, MAX_STEPS);
    validate_within(schema, value, budget)
}

/// [`validate`] with an explicit step budget.
fn validate_within(schema: &Value, value: &Value, budget: usize) -> Vec<String> {
    let mut checker = Checker {
        root: schema,
        steps: 0,
        budget,
        exhausted: false,
        memo: HashMap::new(),
        reported: HashSet::new(),
    };
    let mut sink = Sink::collect();
    checker.check(schema, value, &Path::root(), &[], &mut sink);
    if checker.exhausted {
        // What was found before the budget ran out may be incomplete, and an
        // alternative cut short is no verdict: say only why checking stopped.
        return vec![format!(
            "$: checking stopped after {budget} steps without a verdict: the schema's alternatives (anyOf, oneOf, \
             allOf) and $refs make a value like this too costly to check; send a smaller or simpler value"
        )];
    }
    sink.list
}

/// How many JSON values `value` holds, itself included.
fn count_values(value: &Value) -> usize {
    let mut n = 0;
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        n += 1;
        match v {
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.values()),
            _ => {}
        }
    }
    n
}

/// Why `schema` cannot be used as an agent's output schema, if it cannot. A
/// tool's input must be a JSON object on both wire formats, so the answer is
/// an object too. A keyword of the wrong type (`"required": "name"`, a
/// negative `maxItems`, a `$ref` to a definition that does not exist) is
/// refused here, at boot, rather than silently ignored on every answer; so is
/// a `$ref` graph the checker could not follow: a cycle that describes no part
/// of the value, or a chain of more than [`MAX_REFS_IN_PLACE`] `$ref`s at one
/// place.
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
    malformed(schema, schema, "#").and_then(|()| ref_graph(schema)).err()
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

fn is_container(v: &Value) -> bool {
    matches!(v, Value::Object(_) | Value::Array(_))
}

/// A number held as a double outside the range where JSON integers are kept
/// exactly (`-2^63` to `2^64 - 1`): it may be an integer the parser rounded
/// (`18446744073709551617` is held as `2^64`), so an equality it takes part
/// in may hold only after rounding. `-2^63` itself counts: as a double it is
/// also what `-9223372036854775809` rounds to.
fn rounded(n: &Number) -> bool {
    // 2^64 and -2^63, exactly.
    const ABOVE: f64 = 18_446_744_073_709_551_616.0;
    const BELOW: f64 = -9_223_372_036_854_775_808.0;
    exact_int(n).is_none() && n.as_f64().is_some_and(|f| f >= ABOVE || f <= BELOW)
}

/// Whether `v` holds a [`rounded`] number anywhere.
fn holds_rounded(v: &Value) -> bool {
    match v {
        Value::Number(n) => rounded(n),
        Value::Array(items) => items.iter().any(holds_rounded),
        Value::Object(map) => map.values().any(holds_rounded),
        _ => false,
    }
}

/// The problem for an equality that holds only as doubles.
fn only_as_doubles(what: &str) -> String {
    format!(
        "is equal to {what} only as a double: an integer outside {} to {} is held rounded, so it cannot be compared \
         exactly; send a number inside that range, or a string",
        i64::MIN,
        u64::MAX
    )
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

/// Where in the value a check is, built only when a problem is reported:
/// `$`, `$.key`, `$.list[3]`.
#[derive(Clone, Copy)]
enum Seg<'a> {
    Root,
    Key(&'a str),
    Index(usize),
}

#[derive(Clone, Copy)]
struct Path<'a> {
    parent: Option<&'a Path<'a>>,
    seg: Seg<'a>,
    depth: usize,
}

impl<'a> Path<'a> {
    fn root() -> Path<'static> {
        Path { parent: None, seg: Seg::Root, depth: 0 }
    }

    fn key(&'a self, key: &'a str) -> Path<'a> {
        Path { parent: Some(self), seg: Seg::Key(key), depth: self.depth + 1 }
    }

    fn index(&'a self, i: usize) -> Path<'a> {
        Path { parent: Some(self), seg: Seg::Index(i), depth: self.depth + 1 }
    }

    fn render(&self) -> String {
        use std::fmt::Write;
        let mut segs = Vec::with_capacity(self.depth + 1);
        let mut node = Some(self);
        while let Some(p) = node {
            segs.push(p.seg);
            node = p.parent;
        }
        let mut out = String::new();
        for seg in segs.iter().rev() {
            match seg {
                Seg::Root => out.push('$'),
                Seg::Key(k) => {
                    out.push('.');
                    out.push_str(k);
                }
                Seg::Index(i) => {
                    let _ = write!(out, "[{i}]");
                }
            }
        }
        out
    }
}

/// Where the problems of one check go.
///
/// The main check collects up to [`MAX_PROBLEMS`], in the order the value is
/// walked. A *try* of one alternative (`anyOf`/`oneOf`) stops at its first
/// problem, looks at a value's scalar properties before descending into its
/// objects and arrays (so a wrong discriminator such as `"kind": "sub"` ends
/// it before any subtree is walked), and formats no message unless one is
/// wanted for a report.
struct Sink {
    list: Vec<String>,
    found: usize,
    limit: usize,
    /// A try of one alternative rather than the main check.
    trial: bool,
    /// Format each problem (always, in the main check).
    keep: bool,
    /// How deep in the value the first problem is.
    first: Option<usize>,
}

impl Sink {
    fn collect() -> Self {
        Sink { list: Vec::new(), found: 0, limit: MAX_PROBLEMS, trial: false, keep: true, first: None }
    }

    fn trial(keep: bool) -> Self {
        Sink { list: Vec::new(), found: 0, limit: 1, trial: true, keep, first: None }
    }

    fn full(&self) -> bool {
        self.found >= self.limit
    }

    fn push(&mut self, at: &Path<'_>, message: impl FnOnce() -> String) {
        self.push_as(at, at.depth, message);
    }

    /// A problem at `at`, counted as being `depth` deep (how far into the
    /// value an alternative got before failing: see [`Checker::no_alternative`]).
    fn push_as(&mut self, at: &Path<'_>, depth: usize, message: impl FnOnce() -> String) {
        if self.full() {
            return;
        }
        self.found += 1;
        self.first.get_or_insert(depth);
        if self.keep {
            self.list.push(format!("{}: {}", at.render(), message()));
        }
    }

    /// A problem known only from a remembered verdict (a try without messages).
    fn mark(&mut self, depth: usize) {
        if !self.full() {
            self.found += 1;
            self.first.get_or_insert(depth);
        }
    }
}

/// A remembered verdict of a `$ref` target on one object or array.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Valid,
    /// Invalid, with the first problem this many levels below the place.
    Invalid(usize),
}

struct Checker<'s> {
    root: &'s Value,
    steps: usize,
    budget: usize,
    exhausted: bool,
    /// `(target, value)` → verdict, by address: both live as long as the check.
    memo: HashMap<(usize, usize), Verdict>,
    /// `(target, value)` pairs whose problems are already in the main list.
    reported: HashSet<(usize, usize)>,
}

impl<'s> Checker<'s> {
    /// `refs` are the references followed at this place in the value so far:
    /// it is emptied whenever the check descends into a property or an item,
    /// so a recursive type may recurse as deep as the value does, but a
    /// reference that comes back to itself without consuming anything stops.
    fn check(&mut self, schema: &'s Value, value: &Value, at: &Path<'_>, refs: &[&'s str], sink: &mut Sink) {
        if self.exhausted || sink.full() {
            return;
        }
        self.steps += 1;
        if self.steps > self.budget {
            self.exhausted = true;
            return;
        }
        let Some(s) = schema.as_object() else {
            // `true` and `{}` accept anything; `false` accepts nothing.
            if schema == &Value::Bool(false) {
                sink.push(at, || "no value is allowed here".into());
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
                sink.push(at, || format!("the schema's $ref {reference:?} refers back to itself here"));
            } else if refs.len() >= MAX_REFS_IN_PLACE {
                sink.push(at, || format!("the schema follows more than {MAX_REFS_IN_PLACE} $refs here"));
            } else {
                let mut chain = refs.to_vec();
                chain.push(reference.as_str());
                self.follow(target, value, at, &chain, sink);
            }
            if sink.full() {
                return;
            }
        }

        match s.get("type") {
            Some(Value::String(t)) if !is_type(value, t) => {
                sink.push(at, || format!("expected {t}, got {}", type_name(value)));
                return;
            }
            Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(|t| is_type(value, t)) => {
                sink.push(at, || {
                    let names: Vec<&str> = ts.iter().filter_map(Value::as_str).collect();
                    format!("expected one of {}, got {}", names.join(" | "), type_name(value))
                });
                return;
            }
            Some(t) => {
                // An integer beyond the 64-bit range arrives rounded, so it is
                // not the integer that was sent.
                let names = |name: &str| match t {
                    Value::String(x) => x == name,
                    Value::Array(xs) => xs.iter().any(|x| x == name),
                    _ => false,
                };
                if let Value::Number(n) = value
                    && rounded(n)
                    && names("integer")
                    && !names("number")
                {
                    sink.push(at, || {
                        format!(
                            "expected integer, got {n}, which is outside the integers the runtime carries exactly \
                             ({} to {}); send a number inside that range, or a string",
                            i64::MIN,
                            u64::MAX
                        )
                    });
                    return;
                }
            }
            None => {}
        }

        // A value that is not the constant (or not on the list) is wrong as a
        // whole: what is inside it would only add noise, and in a union this
        // is the discriminator that rules an alternative out.
        if let Some(Value::Array(allowed)) = s.get("enum") {
            let mut unsure = false;
            let listed = allowed.iter().any(|a| match same(a, value) {
                Some(yes) => yes,
                None => {
                    unsure = true;
                    false
                }
            });
            if !listed {
                sink.push(at, || {
                    if unsure {
                        only_as_doubles("one of the allowed values")
                    } else {
                        let shown: Vec<String> = allowed.iter().take(12).map(Value::to_string).collect();
                        format!("must be one of {}", shown.join(", "))
                    }
                });
                return;
            }
        }
        if let Some(c) = s.get("const") {
            match same(c, value) {
                Some(true) => {}
                Some(false) => {
                    sink.push(at, || format!("must be {c}"));
                    return;
                }
                None => {
                    sink.push(at, || only_as_doubles(&c.to_string()));
                    return;
                }
            }
        }

        match value {
            Value::Object(map) => {
                if let Some(Value::Array(required)) = s.get("required") {
                    for name in required.iter().filter_map(Value::as_str) {
                        if !map.contains_key(name) {
                            sink.push(at, || format!("missing required property {name:?}"));
                            if sink.full() {
                                return;
                            }
                        }
                    }
                }
                let props = s.get("properties").and_then(Value::as_object);
                // Which keys `additionalProperties` covers depends on the
                // patterns, which are not evaluated; applying it anyway would
                // refuse keys the patterns allow.
                let patterned = s.contains_key("patternProperties");
                let extra = s.get("additionalProperties");
                if sink.trial {
                    // Scalars first: a discriminator settles a try before any
                    // subtree is walked.
                    for scalars in [true, false] {
                        for (key, item) in map.iter().filter(|(_, item)| is_container(item) != scalars) {
                            self.property(props, patterned, extra, key, item, at, sink);
                            if sink.full() {
                                return;
                            }
                        }
                    }
                } else {
                    for (key, item) in map {
                        self.property(props, patterned, extra, key, item, at, sink);
                        if sink.full() {
                            return;
                        }
                    }
                }
            }
            Value::Array(items) => {
                let n = items.len() as u64;
                if let Some(min) = count_keyword(s, "minItems")
                    && n < min
                {
                    sink.push(at, || format!("needs at least {min} item(s), has {n}"));
                }
                if let Some(max) = count_keyword(s, "maxItems")
                    && n > max
                {
                    sink.push(at, || format!("allows at most {max} item(s), has {n}"));
                }
                if s.get("uniqueItems") == Some(&Value::Bool(true))
                    && let Some((first, again)) = first_duplicate(items)
                {
                    sink.push(at, || {
                        if holds_rounded(&items[first]) || holds_rounded(&items[again]) {
                            format!(
                                "items {first} and {again} are equal only as doubles: an integer outside {} to {} \
                                 is held rounded, so they cannot be told apart; send numbers inside that range, or \
                                 strings",
                                i64::MIN,
                                u64::MAX
                            )
                        } else {
                            format!("items must be unique, but items {first} and {again} are equal")
                        }
                    });
                }
                if sink.full() {
                    return;
                }
                let prefix = s.get("prefixItems").and_then(Value::as_array);
                for (i, item) in items.iter().enumerate() {
                    let here = at.index(i);
                    match prefix.and_then(|p| p.get(i)) {
                        Some(sub) => self.check(sub, item, &here, &[], sink),
                        None => {
                            if let Some(sub) = s.get("items") {
                                self.check(sub, item, &here, &[], sink);
                            }
                        }
                    }
                    if sink.full() {
                        return;
                    }
                }
            }
            Value::Number(num) => {
                for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
                    if let Some(bound) = s.get(key).and_then(Value::as_number)
                        && let Some(problem) = bound_problem(num, key, bound)
                    {
                        sink.push(at, || problem);
                    }
                }
            }
            Value::String(text) => {
                let n = text.chars().count() as u64;
                if let Some(min) = count_keyword(s, "minLength")
                    && n < min
                {
                    sink.push(at, || format!("must be at least {min} character(s) long"));
                }
                if let Some(max) = count_keyword(s, "maxLength")
                    && n > max
                {
                    sink.push(at, || format!("must be at most {max} character(s) long"));
                }
            }
            _ => {}
        }
        if sink.full() {
            return;
        }

        if let Some(Value::Array(all)) = s.get("allOf") {
            for sub in all {
                self.check(sub, value, at, refs, sink);
                if sink.full() {
                    return;
                }
            }
        }
        for key in ["anyOf", "oneOf"] {
            let Some(Value::Array(options)) = s.get(key) else { continue };
            // anyOf is settled by the first match, oneOf by the second.
            let enough = if key == "oneOf" { 2 } else { 1 };
            let mut matched: Vec<usize> = Vec::with_capacity(2);
            // Each alternative that failed, with how deep its first problem is.
            let mut failed: Vec<(usize, usize)> = Vec::new();
            for (i, sub) in options.iter().enumerate() {
                let mut trial = Sink::trial(false);
                self.check(sub, value, at, refs, &mut trial);
                if self.exhausted {
                    return;
                }
                if trial.found == 0 {
                    matched.push(i);
                    if matched.len() == enough {
                        break;
                    }
                } else {
                    failed.push((i, trial.first.unwrap_or(at.depth)));
                }
            }
            if matched.is_empty() {
                self.no_alternative(key, options, &failed, value, at, refs, sink);
            } else if matched.len() > 1 {
                let (a, b) = (matched[0] + 1, matched[1] + 1);
                sink.push(at, || {
                    format!("matches more than one alternative in oneOf (alternatives {a} and {b}); exactly one is allowed")
                });
            }
            if sink.full() {
                return;
            }
        }
    }

    /// Check one property of an object against its schema.
    #[allow(clippy::too_many_arguments)]
    fn property(
        &mut self,
        props: Option<&'s Map<String, Value>>,
        patterned: bool,
        extra: Option<&'s Value>,
        key: &str,
        item: &Value,
        at: &Path<'_>,
        sink: &mut Sink,
    ) {
        let here = at.key(key);
        match props.and_then(|p| p.get(key)) {
            Some(sub) => self.check(sub, item, &here, &[], sink),
            None if patterned => {}
            None => match extra {
                Some(Value::Bool(false)) => sink.push(at, || format!("unexpected property {key:?}")),
                Some(sub @ Value::Object(_)) => self.check(sub, item, &here, &[], sink),
                _ => {}
            },
        }
    }

    /// Check `value` against the `$ref` target `target`, remembering the
    /// verdict for an object or an array: the same definition meets the same
    /// part of the value once per union member that reaches it, and without
    /// this a recursive union re-checked every subtree once per path to it.
    fn follow(&mut self, target: &'s Value, value: &Value, at: &Path<'_>, chain: &[&'s str], sink: &mut Sink) {
        if sink.full() {
            return;
        }
        // The verdict does not depend on how the target was reached: the boot
        // check refuses the in-place cycles and long chains that would make
        // `chain` matter.
        let key = is_container(value).then_some((target as *const Value as usize, value as *const Value as usize));
        if let Some(key) = key {
            match self.memo.get(&key).copied() {
                Some(Verdict::Valid) => return,
                Some(Verdict::Invalid(below)) if sink.trial && !sink.keep => {
                    sink.mark(at.depth + below);
                    return;
                }
                _ if !sink.trial && self.reported.contains(&key) => return,
                _ => {}
            }
        }
        let before = sink.found;
        self.check(target, value, at, chain, sink);
        let Some(key) = key else { return };
        if self.exhausted {
            return;
        }
        if self.memo.len() >= MAX_MEMO {
            // Past the cap nothing more is remembered; already-reported
            // problems are still not repeated.
            if sink.found != before && !sink.trial {
                self.reported.insert(key);
            }
        } else if sink.found == before {
            self.memo.insert(key, Verdict::Valid);
        } else if sink.trial {
            let below = sink.first.unwrap_or(at.depth).saturating_sub(at.depth);
            self.memo.entry(key).or_insert(Verdict::Invalid(below));
        } else {
            self.reported.insert(key);
        }
    }

    /// Report a value that matched none of a union's alternatives in a way a
    /// model can act on.
    ///
    /// Each alternative was tried until its first problem, scalars before
    /// subtrees, so how deep that problem is says how far the value got into
    /// it. When one alternative got strictly furthest — past its type, its
    /// discriminator and its required names — it is the one the value meant,
    /// and its own problems are reported: for an expression tree that is
    /// `$.expr.left.value: expected integer`, not "matches none of the 3
    /// alternatives" at the top. Otherwise the first problem of each of the
    /// first three alternatives is listed.
    #[allow(clippy::too_many_arguments)]
    fn no_alternative(
        &mut self,
        key: &str,
        options: &'s [Value],
        failed: &[(usize, usize)],
        value: &Value,
        at: &Path<'_>,
        refs: &[&'s str],
        sink: &mut Sink,
    ) {
        let n = options.len();
        let furthest = failed.iter().map(|f| f.1).max().unwrap_or(at.depth);
        if sink.trial {
            // Only the verdict matters in a try, and how far the value got,
            // for the union around this one to compare.
            sink.push_as(at, furthest, || format!("matches none of the {n} alternatives in {key}"));
            return;
        }
        if let [(only, _)] = failed.iter().filter(|f| f.1 == furthest).collect::<Vec<_>>()[..] {
            let before = sink.found;
            self.check(&options[*only], value, at, refs, sink);
            if sink.found > before || self.exhausted {
                return;
            }
        }
        let mut detail = Vec::new();
        for &(i, _) in failed.iter().take(3) {
            let mut first = Sink::trial(true);
            self.check(&options[i], value, at, refs, &mut first);
            if self.exhausted {
                return;
            }
            if let Some(problem) = first.list.pop() {
                detail.push(format!("{}. {problem}", i + 1));
            }
        }
        let more = if failed.len() > 3 { "; …" } else { "" };
        sink.push(at, || {
            if detail.is_empty() {
                format!("matches none of the {n} alternatives in {key}")
            } else {
                format!("matches none of the {n} alternatives in {key} ({}{more})", detail.join("; "))
            }
        });
    }
}

/// The problem with `num` against the bound keyword `key`, if any. An
/// equality that holds only between doubles, one of them beyond the exact
/// integer range, is no verdict either way, and is refused.
fn bound_problem(num: &Number, key: &str, bound: &Number) -> Option<String> {
    let ord = num_cmp(num, bound);
    if ord == Some(Ordering::Equal) && (rounded(num) || rounded(bound)) {
        return Some(only_as_doubles(&format!("{key} {bound}")));
    }
    let (holds, op) = match key {
        "minimum" => (ord != Some(Ordering::Less), ">="),
        "maximum" => (ord != Some(Ordering::Greater), "<="),
        "exclusiveMinimum" => (ord == Some(Ordering::Greater), ">"),
        _ => (ord == Some(Ordering::Less), "<"),
    };
    (!holds).then(|| format!("must be {op} {bound}"))
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
/// same number, in an array or an object as much as on their own. `None` when
/// the two are equal only as doubles and one of the numbers that decides it is
/// [`rounded`]: they may be two different integers.
fn same(a: &Value, b: &Value) -> Option<bool> {
    // Equal throughout, unless some part is unsure; any part that differs
    // settles it.
    fn all<'x>(pairs: impl Iterator<Item = (&'x Value, &'x Value)>) -> Option<bool> {
        let mut unsure = false;
        for (p, q) in pairs {
            match same(p, q) {
                Some(true) => {}
                Some(false) => return Some(false),
                None => unsure = true,
            }
        }
        if unsure { None } else { Some(true) }
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match num_cmp(x, y) {
            Some(Ordering::Equal) if rounded(x) || rounded(y) => None,
            ord => Some(ord == Some(Ordering::Equal)),
        },
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            all(x.iter().zip(y))
        }
        (Value::Object(x), Value::Object(y)) => {
            if x.len() != y.len() || x.keys().any(|k| !y.contains_key(k)) {
                return Some(false);
            }
            all(x.iter().map(|(k, v)| (v, &y[k])))
        }
        _ => Some(a == b),
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

/// The `$ref` graph, as the checker will follow it. Refused:
///
/// - a cycle of `$ref`s that apply at the same place in the value (directly,
///   or through `allOf`/`anyOf`/`oneOf`) without describing any part of it,
///   whether it comes back to a `$defs` entry or to a sub-schema inside one
///   (`#/$defs/D/allOf/0` pointing at itself): checking it would never get
///   anywhere;
/// - a chain of more than [`MAX_REFS_IN_PLACE`] such `$ref`s, which the
///   checker would stop following, so that every answer failed.
///
/// Recursion through a property or an item is fine: each step consumes part
/// of the value.
fn ref_graph(root: &Value) -> Result<(), String> {
    let mut refs = Vec::new();
    schema_refs(root, &mut refs);
    let mut longest: HashMap<*const Value, usize> = HashMap::new();
    let mut stack: Vec<*const Value> = Vec::new();
    // A target is read as a schema wherever it points (even somewhere that is
    // not a schema position), so the references inside it count too.
    let mut walked: HashSet<*const Value> = HashSet::new();
    let mut i = 0;
    while let Some(&reference) = refs.get(i) {
        i += 1;
        if let Some(target) = resolve_local(root, reference) {
            chain_from(root, reference, target, &mut longest, &mut stack)?;
            if walked.insert(target as *const Value) {
                schema_refs(target, &mut refs);
            }
        }
    }
    Ok(())
}

/// The most `$ref`s followed in a row at one place once `target` is entered
/// through `reference` (itself included), by depth-first search over the
/// in-place references, remembering each finished target.
fn chain_from<'s>(
    root: &'s Value,
    reference: &str,
    target: &'s Value,
    longest: &mut HashMap<*const Value, usize>,
    stack: &mut Vec<*const Value>,
) -> Result<usize, String> {
    let key = target as *const Value;
    if let Some(&n) = longest.get(&key) {
        return Ok(n);
    }
    if stack.contains(&key) {
        return Err(format!(
            "{reference} refers back to itself through $refs that describe no part of the value, so no answer could \
             ever be checked against it"
        ));
    }
    if stack.len() >= MAX_REFS_IN_PLACE {
        return Err(format!(
            "the schema follows more than {MAX_REFS_IN_PLACE} $refs in a row at one place (the chain reaches \
             {reference:?}) without describing any part of the value, and the checker follows at most \
             {MAX_REFS_IN_PLACE}; shorten the chain"
        ));
    }
    stack.push(key);
    let mut most = 0;
    for next in in_place_refs(target) {
        if let Some(t) = resolve_local(root, next) {
            most = most.max(chain_from(root, next, t, longest, stack)?);
        }
    }
    stack.pop();
    longest.insert(key, most + 1);
    Ok(most + 1)
}

/// Every `$ref` in a place the checker reads a schema from: the root, its
/// `$defs`, and every sub-schema under `properties`, `additionalProperties`,
/// `items`, `prefixItems`, `allOf`, `anyOf` and `oneOf`. (A `const` or an
/// `enum` holds data, not schemas.)
fn schema_refs<'s>(schema: &'s Value, out: &mut Vec<&'s str>) {
    let Some(s) = schema.as_object() else { return };
    if let Some(Value::String(r)) = s.get("$ref") {
        out.push(r);
    }
    for key in ["properties", "$defs"] {
        if let Some(Value::Object(subs)) = s.get(key) {
            for sub in subs.values() {
                schema_refs(sub, out);
            }
        }
    }
    for key in ["additionalProperties", "items"] {
        if let Some(sub) = s.get(key) {
            schema_refs(sub, out);
        }
    }
    for key in ["prefixItems", "allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(subs)) = s.get(key) {
            for sub in subs {
                schema_refs(sub, out);
            }
        }
    }
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

    /// `Add | Sub | Lit`, two records sharing the recursive fields: the shape
    /// that made 2.9.0's checking exponential in the answer's depth.
    fn expr() -> Value {
        let binary = |kind: &str| {
            json!({
                "type": "object",
                "properties": {"kind": {"const": kind}, "left": {"$ref": "#/$defs/E"}, "right": {"$ref": "#/$defs/E"}},
                "required": ["kind", "left", "right"],
                "additionalProperties": false,
            })
        };
        json!({
            "type": "object",
            "properties": {"expr": {"$ref": "#/$defs/E"}},
            "required": ["expr"],
            "$defs": {
                "Add": binary("add"),
                "Sub": binary("sub"),
                "Lit": {"type": "object", "properties": {"kind": {"const": "lit"}, "value": {"type": "integer"}},
                        "required": ["kind", "value"], "additionalProperties": false},
                "E": {"anyOf": [{"$ref": "#/$defs/Add"}, {"$ref": "#/$defs/Sub"}, {"$ref": "#/$defs/Lit"}]},
            },
        })
    }

    /// A chain `depth` deep, alternating add and sub, with `leaf` at the bottom.
    fn expr_chain(depth: usize, leaf: Value) -> Value {
        let mut e = leaf;
        for i in 0..depth {
            let kind = if i % 2 == 0 { "sub" } else { "add" };
            e = json!({"kind": kind, "left": e, "right": {"kind": "lit", "value": 1}});
        }
        json!({"expr": e})
    }

    #[test]
    fn a_recursive_union_is_checked_in_time_linear_in_the_answer() {
        let schema = expr();
        assert_eq!(output_schema_problem(&schema), None);
        // 2.9.0 took minutes at depth 22; this is 120 deep.
        let started = std::time::Instant::now();
        let good = expr_chain(120, json!({"kind": "lit", "value": 0}));
        assert_eq!(validate(&schema, &good), Vec::<String>::new());
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        // The union member the value meant is the one reported, at the place
        // that is wrong, not "matches none" at the top.
        let bad = expr_chain(3, json!({"kind": "lit", "value": "zero"}));
        assert_eq!(validate(&schema, &bad), vec!["$.expr.left.left.left.value: expected integer, got string"]);
        let unknown = json!({"expr": {"kind": "mul", "left": {"kind": "lit", "value": 1}, "right": {"kind": "lit", "value": 2}}});
        let problems = validate(&schema, &unknown);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("$.expr: matches none of the 3 alternatives in anyOf (1. $.expr.kind: must be \"add\"; "),
                "{problems:?}");
    }

    #[test]
    fn one_of_stops_at_the_second_match_and_names_both() {
        let s = json!({"oneOf": [{"type": "integer"}, {"type": "string"}, {"type": "number", "minimum": 0},
                                 {"type": "number", "maximum": 9}]});
        assert!(valid(s.clone(), json!("x")));
        assert_eq!(validate(&s, &json!(3)), vec!["$: matches more than one alternative in oneOf (alternatives 1 and 3); exactly one is allowed"]);
    }

    #[test]
    fn a_failed_const_or_enum_is_not_descended_into() {
        let s = json!({"const": {"a": 1}, "properties": {"a": {"type": "string"}}});
        assert_eq!(validate(&s, &json!({"a": 2})), vec!["$: must be {\"a\":1}"]);
        let s = json!({"enum": [[1]], "items": {"type": "string"}});
        assert_eq!(validate(&s, &json!([2])), vec!["$: must be one of [1]"]);
    }

    #[test]
    fn a_schema_whose_alternatives_multiply_stops_at_the_step_budget() {
        // D0 .. D29, each "D(i+1) or D(i+1)": 2^30 ways to check one scalar,
        // all at one place. A chain of 30 in-place $refs boots.
        let mut defs = Map::new();
        for i in 0..30 {
            let next = json!({"$ref": format!("#/$defs/D{}", i + 1)});
            defs.insert(format!("D{i}"), json!({"anyOf": [next.clone(), {"allOf": [next]}]}));
        }
        defs.insert("D30".into(), json!({"type": "string"}));
        let schema = json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/D0"}}, "$defs": defs});
        assert_eq!(output_schema_problem(&schema), None);
        let started = std::time::Instant::now();
        let problems = validate(&schema, &json!({"x": 1}));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with(&format!("$: checking stopped after {MIN_STEPS} steps without a verdict")), "{problems:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
        // An answer the first alternatives settle is still accepted.
        assert!(validate(&schema, &json!({"x": "s"})).is_empty());
        // The budget grows with the value.
        assert_eq!(validate_within(&json!({"type": "integer"}), &json!(1), 0).len(), 1);
        assert!(validate_within(&json!({"type": "integer"}), &json!(1), 1).is_empty());
    }

    #[test]
    fn integers_beyond_64_bits_are_never_passed_on_a_rounding() {
        let big = json!(18_446_744_073_709_551_616.0); // what 2^64 + 1 parses to
        let int = json!({"type": "integer"});
        let p = validate(&int, &big);
        assert_eq!(p.len(), 1);
        assert!(p[0].starts_with("$: expected integer, got 1.8446744073709552e+19, which is outside the integers"), "{p:?}");
        assert!(!valid(json!({"type": ["integer", "null"]}), big.clone()));
        assert!(valid(json!({"type": ["integer", "number"]}), big.clone()));
        assert!(valid(json!({"type": "number"}), big.clone()));
        assert!(!valid(int.clone(), json!(-9_223_372_036_854_775_808.0)));
        assert!(valid(int.clone(), json!(i64::MIN)));
        assert!(valid(int.clone(), json!(u64::MAX)));
        assert!(valid(int, json!(9_223_372_036_854_775_808.0)));
        // An equality that holds only as doubles is no verdict.
        let p = validate(&json!({"maximum": 18_446_744_073_709_551_616.0}), &big);
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("is equal to maximum 1.8446744073709552e+19 only as a double"), "{p:?}");
        assert!(!valid(json!({"minimum": 18_446_744_073_709_551_616.0}), big.clone()));
        assert!(!valid(json!({"const": 18_446_744_073_709_551_616.0}), big.clone()));
        assert!(!valid(json!({"enum": [1, 18_446_744_073_709_551_616.0]}), big.clone()));
        assert!(!valid(json!({"const": i64::MIN}), json!(-9_223_372_036_854_775_808.0)));
        assert!(!valid(json!({"const": [1, 18_446_744_073_709_551_616.0]}), json!([1, 18_446_744_073_709_551_616.0])));
        // Strict order is exact either way, and a part that differs settles it.
        assert!(valid(json!({"maximum": 1e30}), big.clone()));
        assert!(!valid(json!({"maximum": u64::MAX}), big.clone()));
        assert!(!valid(json!({"const": [2, 18_446_744_073_709_551_616.0]}), json!([1, 18_446_744_073_709_551_616.0])));
        assert!(valid(json!({"enum": [18_446_744_073_709_551_616.0, 5]}), json!(5)));
        let p = validate(&json!({"uniqueItems": true}), &json!([1e20, 1e20]));
        assert_eq!(p.len(), 1);
        assert!(p[0].starts_with("$: items 0 and 1 are equal only as doubles"), "{p:?}");
        assert!(valid(json!({"uniqueItems": true}), json!([1e20, 2e20])));
    }

    #[test]
    fn boot_refuses_ref_graphs_the_checker_could_not_follow() {
        let chain = |n: usize| {
            let mut defs = Map::new();
            for i in 0..n - 1 {
                defs.insert(format!("D{i}"), json!({"$ref": format!("#/$defs/D{}", i + 1)}));
            }
            defs.insert(format!("D{}", n - 1), json!({"type": "integer"}));
            json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/D0"}}, "$defs": defs})
        };
        // 32 in a row is what the checker follows; 33 it would refuse on every answer.
        assert_eq!(output_schema_problem(&chain(MAX_REFS_IN_PLACE)), None);
        assert!(validate(&chain(MAX_REFS_IN_PLACE), &json!({"x": 3})).is_empty());
        let long = output_schema_problem(&chain(MAX_REFS_IN_PLACE + 1)).expect("33 refused");
        assert!(long.contains("more than 32 $refs in a row at one place"), "{long}");
        // A sub-schema pointing at itself consumes nothing.
        let selfptr = json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/D0"}},
                             "$defs": {"D0": {"allOf": [{"$ref": "#/$defs/D0/allOf/0"}]}}});
        let why = output_schema_problem(&selfptr).expect("self-pointing refused");
        assert!(why.starts_with("#/$defs/D0/allOf/0 refers back to itself"), "{why}");
        // So does a cycle through two definitions, and one reached only
        // through a target that is not a schema position.
        let two = json!({"type": "object", "$defs": {"A": {"anyOf": [{"$ref": "#/$defs/B"}]}, "B": {"allOf": [{"$ref": "#/$defs/A"}]}}});
        assert!(output_schema_problem(&two).unwrap().contains("refers back to itself"));
        let hidden = json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/A/const"}},
                            "$defs": {"A": {"const": {"anyOf": [{"$ref": "#/$defs/A/const"}]}}}});
        assert!(output_schema_problem(&hidden).unwrap().contains("refers back to itself"));
        // Recursion through a property is fine, however deep.
        assert_eq!(output_schema_problem(&tree()), None);
        assert_eq!(output_schema_problem(&expr()), None);
    }
}
