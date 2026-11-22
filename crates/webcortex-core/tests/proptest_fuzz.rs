//! Property-based tests (proptest) for the parsers and checkers 2.9.0 hardened:
//! the router's path normalisation and parameter decoding, the structured-output
//! validator (`schema.rs`), template escaping and the `json` filter, the error
//! catalogue, `OTEL_EXPORTER_OTLP_HEADERS` parsing, query-string parsing and
//! JWT acceptance.
//!
//! Each property compares the runtime with an oracle written here from the
//! documented behaviour, not from the implementation. Case counts scale with
//! `PROPTEST_CASES` (default 256 per property; heavier properties run fewer).
//! Run: `cargo test -p webcortex-core --test proptest_fuzz`.
//!
//! Deterministic by default: every property starts from a fixed seed, so a
//! run in CI is the same run each time. `PROPTEST_RNG_SEED=<n>` picks another
//! seed and `PROPTEST_RANDOM=1` a fresh one per run (with a large
//! `PROPTEST_CASES`, that is a fuzzing campaign).
//!
//! Six of these failed on 2.9.0 and were `#[ignore]`d with the defect named;
//! 2.9.1 fixed them (double-decoded path parameters, exponential checking of
//! recursive unions, boot accepting `$ref` graphs the checker refuses).

use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, RngSeed};
use serde_json::{Map, Number, Value, json};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use webcortex_core::auth::{Authenticator, JWT_SUBJECT_UNSET};
use webcortex_core::errors;
use webcortex_core::http::{WebCortexResponse, parse_query_pairs};
use webcortex_core::manifest::{AuthConfig, Route, TemplateConfig, TracingConfig};
use webcortex_core::router::{MatchError, Router};
use webcortex_core::schema::{output_schema_problem, validate};
use webcortex_core::templates::Templates;
use webcortex_core::trace::Tracer;

// ----------------------------------------------------------------- config

fn cases(scale: f64) -> ProptestConfig {
    let base: f64 = std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(256.0);
    let defaults = ProptestConfig::default();
    // PROPTEST_RNG_SEED (read into the defaults) wins; otherwise a fixed seed.
    let rng_seed = match defaults.rng_seed {
        RngSeed::Random if std::env::var_os("PROPTEST_RANDOM").is_none() => RngSeed::Fixed(0x5EED_0291),
        seed => seed,
    };
    ProptestConfig {
        cases: ((base * scale).max(16.0)) as u32,
        // Failures are reported with their minimal input; no regression files
        // are written into the source tree.
        failure_persistence: None,
        rng_seed,
        ..defaults
    }
}

// ------------------------------------------------------------- strategies

fn special_numbers() -> Vec<Number> {
    let mut v: Vec<Number> = vec![
        Number::from(0),
        Number::from(1),
        Number::from(-1),
        Number::from(9_007_199_254_740_992i64), // 2^53
        Number::from(9_007_199_254_740_993i64), // 2^53 + 1
        Number::from(i64::MAX),
        Number::from(i64::MIN),
        Number::from(u64::MAX),
        Number::from(1u64 << 63),
    ];
    for f in [
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        2.0,
        9_007_199_254_740_992.0,
        9_223_372_036_854_775_808.0, // 2^63
        -9_223_372_036_854_775_808.0,
        18_446_744_073_709_551_616.0, // 2^64
        1e300,
        -1e300,
        f64::MIN_POSITIVE,
        5e-324,
        f64::MAX,
        f64::MIN,
    ] {
        v.push(Number::from_f64(f).unwrap());
    }
    v
}

fn arb_number() -> impl Strategy<Value = Number> {
    prop_oneof![
        3 => (-4i64..5).prop_map(Number::from),
        3 => (-4i64..5).prop_map(|i| Number::from_f64(i as f64).unwrap()),
        2 => any::<i64>().prop_map(Number::from),
        1 => any::<u64>().prop_map(Number::from),
        2 => any::<f64>().prop_filter_map("finite", Number::from_f64),
        2 => prop::sample::select(special_numbers()),
        1 => (-1_000_000i64..1_000_000, 0u32..4).prop_map(|(m, e)| Number::from_f64(m as f64 / 10f64.powi(e as i32)).unwrap()),
    ]
}

fn arb_text() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[a-c]{0,3}",
        2 => any::<String>(),
        2 => prop::sample::select(vec![
            "", "<", ">", "&", "'", "\"", "/", "</script>", "<!--", "\u{2028}", "\u{2029}", "é", "😀",
            "\\", "\u{0}", "\n", "]]>", "&amp;", "{{", "%",
        ])
        .prop_map(str::to_string),
        1 => prop::collection::vec(prop::sample::select(vec!['<', '>', '&', '\'', '"', '/', 'a', '\u{2028}', '\\', 'é']), 0..12)
            .prop_map(|cs| cs.into_iter().collect()),
    ]
}

fn arb_key() -> impl Strategy<Value = String> {
    prop_oneof![4 => "[a-d]", 1 => arb_text()]
}

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        1 => Just(Value::Null),
        1 => any::<bool>().prop_map(Value::Bool),
        4 => arb_number().prop_map(Value::Number),
        2 => arb_text().prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 40, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(arb_key(), inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect::<Map<String, Value>>())),
        ]
    })
}

// ------------------------------------------------------- exact-number oracle

/// A JSON number as sign × mantissa × 2^exp, exactly.
fn decompose(n: &Number) -> (bool, u128, i32) {
    if let Some(i) = n.as_i64() {
        return (i < 0, i.unsigned_abs() as u128, 0);
    }
    if let Some(u) = n.as_u64() {
        return (false, u as u128, 0);
    }
    let f = n.as_f64().expect("finite");
    let bits = f.to_bits();
    let neg = bits >> 63 == 1;
    let e = ((bits >> 52) & 0x7ff) as i32;
    let frac = (bits & ((1u64 << 52) - 1)) as u128;
    if e == 0 { (neg, frac, -1074) } else { (neg, frac | (1u128 << 52), e - 1075) }
}

fn cmp_mag(m1: u128, e1: i32, m2: u128, e2: i32) -> Ordering {
    match (m1 == 0, m2 == 0) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    if e1 >= e2 {
        let d = (e1 - e2) as u32;
        if d > m1.leading_zeros() { Ordering::Greater } else { (m1 << d).cmp(&m2) }
    } else {
        cmp_mag(m2, e2, m1, e1).reverse()
    }
}

/// Exact comparison of two JSON numbers, as JSON Schema means them.
fn exact_cmp(a: &Number, b: &Number) -> Ordering {
    let (na, ma, ea) = decompose(a);
    let (nb, mb, eb) = decompose(b);
    let za = ma == 0;
    let zb = mb == 0;
    let sa = if za { 0 } else if na { -1 } else { 1 };
    let sb = if zb { 0 } else if nb { -1 } else { 1 };
    if sa != sb {
        return sa.cmp(&sb);
    }
    match sa {
        0 => Ordering::Equal,
        1 => cmp_mag(ma, ea, mb, eb),
        _ => cmp_mag(mb, eb, ma, ea),
    }
}

fn is_integral(n: &Number) -> bool {
    let (_, m, e) = decompose(n);
    m == 0 || e >= 0 || m.trailing_zeros() >= (-e) as u32
}

/// JSON Schema equality: numbers by value, at any depth.
fn oracle_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => exact_cmp(x, y) == Ordering::Equal,
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| oracle_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| oracle_eq(v, w)))
        }
        _ => a == b,
    }
}

/// A double outside the range where JSON integers are held exactly (-2^63 to
/// 2^64 - 1). It may be an integer the parser rounded (serde_json keeps
/// `18446744073709551617` as the double 2^64), so since 2.9.1 the checker
/// lets neither `type: integer` nor an equality that rests on it pass.
fn rounded(n: &Number) -> bool {
    n.as_i64().is_none()
        && n.as_u64().is_none()
        && n.as_f64().is_some_and(|f| f >= 18_446_744_073_709_551_616.0 || f <= -9_223_372_036_854_775_808.0)
}

/// [`oracle_eq`] under that rule: `None` when the two are equal only through
/// a rounded number (they may be two different integers). A part that
/// differs settles it either way.
fn oracle_same(a: &Value, b: &Value) -> Option<bool> {
    fn combine(parts: impl Iterator<Item = Option<bool>>) -> Option<bool> {
        let parts: Vec<Option<bool>> = parts.collect();
        if parts.contains(&Some(false)) {
            Some(false)
        } else if parts.contains(&None) {
            None
        } else {
            Some(true)
        }
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let eq = exact_cmp(x, y) == Ordering::Equal;
            if eq && (rounded(x) || rounded(y)) { None } else { Some(eq) }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => combine(x.iter().zip(y).map(|(p, q)| oracle_same(p, q))),
        (Value::Object(x), Value::Object(y)) if x.len() == y.len() && x.keys().all(|k| y.contains_key(k)) => {
            combine(x.iter().map(|(k, v)| oracle_same(v, &y[k])))
        }
        (Value::Array(_), Value::Array(_)) | (Value::Object(_), Value::Object(_)) => Some(false),
        _ => Some(a == b),
    }
}

fn ok(schema: &Value, value: &Value) -> bool {
    validate(schema, value).is_empty()
}

// ======================================================================
// Router
// ======================================================================

fn route(id: u32, method: &str, path: &str) -> Route {
    serde_json::from_value(json!({
        "id": id, "method": method, "path": path,
        "op": {"kind": "static", "status": 200, "body": {}},
    }))
    .expect("route")
}

fn router() -> &'static Router {
    static R: OnceLock<Router> = OnceLock::new();
    R.get_or_init(|| {
        Router::build(&[
            route(0, "GET", "/r/{x}"),
            route(1, "GET", "/f/{*rest}"),
            route(2, "GET", "/a/{y}/{z}"),
            route(3, "GET", "/a/{x}"),
            route(4, "POST", "/r/{x}"),
        ])
        .expect("router")
    })
}

const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Encode `value` one character at a time, each either raw (when it may stand
/// raw in a path segment) or as %XX in random case.
fn encode_segment(value: &str, choices: &[u8]) -> String {
    let mut out = String::new();
    for (i, c) in value.chars().enumerate() {
        let choice = choices.get(i % choices.len().max(1)).copied().unwrap_or(0);
        let raw_ok = c.is_ascii_alphanumeric() || "-._~!$&'()*+,;=:@".contains(c) || !c.is_ascii();
        if raw_ok && choice % 3 == 0 {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                if choice % 2 == 0 {
                    out.push_str(&format!("%{b:02X}"));
                } else {
                    out.push_str(&format!("%{b:02x}"));
                }
            }
        }
    }
    out
}

proptest! {
    #![proptest_config(cases(8.0))]

    /// Any value, percent-encoded any way, comes back from a `{x}` parameter
    /// exactly as it was: encoded slashes, `%25` and non-ASCII included.
    #[test]
    fn router_param_round_trips(value in "\\PC{1,12}", choices in prop::collection::vec(any::<u8>(), 1..8)) {
        let enc = encode_segment(&value, &choices);
        let m = router().find("GET", &format!("/r/{enc}")).map_err(|e| TestCaseError::fail(format!("{e:?} for {enc}")))?;
        prop_assert_eq!(m.route_id, 0);
        prop_assert_eq!(&m.path_params["x"], &value, "sent {}", enc);
    }

    /// A captured parameter is decoded exactly once: what the handler sees is
    /// the single percent-decoding of the segment the client sent (the same
    /// lenient decoding `percent_decode_str` does), whenever that is UTF-8.
    #[test]
    fn router_param_is_decoded_exactly_once(seg in "([0-7g-z.~_-]|%|%[0-7]|%[0-7][0-9A-Fa-f]){1,10}") {
        let single = percent_encoding::percent_decode_str(&seg).decode_utf8();
        prop_assume!(single.is_ok());
        let single = single.unwrap().into_owned();
        let m = router().find("GET", &format!("/r/{seg}")).map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(&m.path_params["x"], &single, "sent /r/{}", seg);
    }

    /// A parameter that does not decode to UTF-8 "is kept as sent, so a
    /// handler sees the original bytes rather than a lossy rewrite"
    /// (router.rs, `decode_param`).
    #[test]
    fn router_undecodable_param_is_kept_as_sent(seg in "([a-z0-9]|%[0-9A-Fa-f]{2}){0,4}%[89a-fA-F][0-9a-fA-F]([a-z0-9]|%[0-9A-Fa-f]{2}){0,4}") {
        let single = percent_encoding::percent_decode_str(&seg).decode_utf8();
        prop_assume!(single.is_err());
        let m = router().find("GET", &format!("/r/{seg}")).map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(&m.path_params["x"], &seg, "sent /r/{}", seg);
    }

    /// `%2F` never becomes a separator: `/a/p%2Fq` reaches `/a/{x}` with
    /// `x = "p/q"`, never the two-parameter route.
    #[test]
    fn router_encoded_slash_never_splits(p in "[a-z0-9é]{1,6}", q in "[a-z0-9é]{1,6}", lower in any::<bool>(), back in any::<bool>()) {
        let sep = match (back, lower) { (false, false) => "%2F", (false, true) => "%2f", (true, false) => "%5C", (true, true) => "%5c" };
        let path = format!("/a/{}{sep}{}", encode_segment(&p, &[1]), encode_segment(&q, &[0]));
        let m = router().find("GET", &path).map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(m.route_id, 3, "{} reached the wrong route", path);
        let want = format!("{p}{}{q}", if back { "\\" } else { "/" });
        prop_assert_eq!(&m.path_params["x"], &want);
    }

    /// A literal route answers however a client spells it: raw UTF-8, %XX in
    /// either case, unreserved characters encoded or not.
    #[test]
    fn router_literal_routes_match_every_spelling(lit in "[a-zA-Z0-9._~é-]{1,8}", choices in prop::collection::vec(any::<u8>(), 1..8)) {
        prop_assume!(lit != "." && lit != "..");
        let r = Router::build(&[route(0, "GET", &format!("/lit/{lit}")), route(1, "GET", "/lit/{other}/x")]).expect("router");
        let path = format!("/lit/{}", encode_segment(&lit, &choices));
        let m = r.find("GET", &path).map_err(|e| TestCaseError::fail(format!("{e:?} for {path}")))?;
        prop_assert_eq!(m.route_id, 0, "{}", path);
    }

    /// Nothing a client can send makes the router panic, and 405 is answered
    /// only where some method is declared for the path.
    #[test]
    fn router_never_panics(path in "/\\PC{0,24}", method in "[A-Za-z]{1,8}") {
        match router().find(&method, &path) {
            Ok(_) => {}
            Err(MatchError::MethodNotAllowed) => prop_assert!(!router().allowed_methods(&path).is_empty(), "405 without an Allow for {}", path),
            Err(MatchError::NotFound) => {}
        }
    }

    /// Declaring arbitrary route text either builds or is refused; it never panics.
    #[test]
    fn router_build_never_panics(paths in prop::collection::vec("/[a-z{}*%é/]{0,12}", 1..5)) {
        let routes: Vec<Route> = paths.iter().enumerate().map(|(i, p)| route(i as u32, "GET", p)).collect();
        let _ = Router::build(&routes);
    }
}

// ======================================================================
// Structured-output validator
// ======================================================================

proptest! {
    #![proptest_config(cases(16.0))]

    /// `minimum`/`maximum` and the exclusive bounds compare exactly, integer
    /// against integer, integer against float and float against float. An
    /// equality through a rounded number is refused, as no verdict.
    #[test]
    fn validator_numeric_bounds_are_exact(v in arb_number(), b in arb_number()) {
        let c = exact_cmp(&v, &b);
        let sure = !(c == Ordering::Equal && (rounded(&v) || rounded(&b)));
        let value = Value::Number(v.clone());
        prop_assert_eq!(ok(&json!({"minimum": b}), &value), sure && c != Ordering::Less, "minimum {} vs {}", b, v);
        prop_assert_eq!(ok(&json!({"maximum": b}), &value), sure && c != Ordering::Greater, "maximum {} vs {}", b, v);
        prop_assert_eq!(ok(&json!({"exclusiveMinimum": b}), &value), sure && c == Ordering::Greater, "exclusiveMinimum {} vs {}", b, v);
        prop_assert_eq!(ok(&json!({"exclusiveMaximum": b}), &value), sure && c == Ordering::Less, "exclusiveMaximum {} vs {}", b, v);
    }

    /// `type: integer` accepts exactly the integral numbers, floats included,
    /// except a rounded one (it is not the integer that was sent).
    #[test]
    fn validator_integer_type_is_integral_value(v in arb_number()) {
        prop_assert_eq!(ok(&json!({"type": "integer"}), &Value::Number(v.clone())), is_integral(&v) && !rounded(&v), "{}", v);
        prop_assert!(ok(&json!({"type": "number"}), &Value::Number(v.clone())), "number refused {}", v);
    }

    /// `const` and `enum` use JSON Schema equality: 1 == 1.0, at any depth;
    /// an equality through a rounded number does not count.
    #[test]
    fn validator_const_and_enum_use_numeric_equality(a in arb_json(), b in arb_json(), extra in prop::collection::vec(arb_json(), 0..3)) {
        prop_assert_eq!(ok(&json!({"const": a}), &b), oracle_same(&a, &b) == Some(true), "const {} vs {}", a, b);
        let mut allowed = extra.clone();
        allowed.push(a.clone());
        let want = allowed.iter().any(|x| oracle_same(x, &b) == Some(true));
        prop_assert_eq!(ok(&json!({"enum": allowed}), &b), want, "enum with {}", b);
    }

    /// `uniqueItems` refuses exactly the arrays with two equal items.
    #[test]
    fn validator_unique_items_matches_pairwise_equality(items in prop::collection::vec(prop_oneof![
        3 => arb_number().prop_map(Value::Number),
        1 => arb_json(),
        1 => prop::collection::vec(arb_number().prop_map(Value::Number), 0..3).prop_map(Value::Array),
        1 => prop::collection::btree_map("[ab]", arb_number().prop_map(Value::Number), 0..3)
                 .prop_map(|m| Value::Object(m.into_iter().collect())),
    ], 0..7)) {
        let dup = (0..items.len()).any(|i| (i + 1..items.len()).any(|j| oracle_eq(&items[i], &items[j])));
        prop_assert_eq!(ok(&json!({"uniqueItems": true}), &Value::Array(items.clone())), !dup, "{:?}", items);
    }

    /// Count keywords count characters (code points) and items, and a count
    /// written as an integral float (`2.0`) means the same as the integer.
    #[test]
    fn validator_counts_accept_integral_floats(s in "\\PC{0,6}", n in 0u64..8, as_float in any::<bool>(), len in 0usize..8) {
        let count = if as_float { json!(n as f64) } else { json!(n) };
        let chars = s.chars().count() as u64;
        let value = Value::String(s.clone());
        prop_assert_eq!(ok(&json!({"minLength": count}), &value), chars >= n, "minLength {} of {:?}", n, s);
        prop_assert_eq!(ok(&json!({"maxLength": count}), &value), chars <= n, "maxLength {} of {:?}", n, s);
        let arr = Value::Array(vec![Value::Null; len]);
        prop_assert_eq!(ok(&json!({"minItems": count}), &arr), len as u64 >= n, "minItems {} of {}", n, len);
        prop_assert_eq!(ok(&json!({"maxItems": count}), &arr), len as u64 <= n, "maxItems {} of {}", n, len);
    }
}

/// A schema built from every keyword, with values of the right type and of
/// the wrong one.
fn arb_schema() -> impl Strategy<Value = Value> {
    let type_name = prop::sample::select(vec!["null", "boolean", "integer", "number", "string", "array", "object", "strin"]);
    let leaf = prop_oneof![
        Just(json!({})),
        Just(json!(true)),
        Just(json!(false)),
        type_name.clone().prop_map(|t| json!({"type": t})),
        prop::collection::vec(type_name, 0..3).prop_map(|ts| json!({"type": ts})),
        arb_json().prop_map(|v| json!({"const": v})),
        prop::collection::vec(arb_json(), 0..3).prop_map(|v| json!({"enum": v})),
        (prop::sample::select(vec!["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"]), arb_json())
            .prop_map(|(k, v)| json!({k: v})),
        (prop::sample::select(vec!["minItems", "maxItems", "minLength", "maxLength"]), arb_json())
            .prop_map(|(k, v)| json!({k: v})),
        arb_json().prop_map(|v| json!({"uniqueItems": v})),
        arb_json().prop_map(|v| json!({"nullable": v})),
        prop::sample::select(vec!["#/$defs/A", "#/$defs/B", "#/$defs/Missing", "#", "#/$defs/A/properties/a", "http://x/y", "#/$defs/%41"])
            .prop_map(|r| json!({"$ref": r})),
        arb_json().prop_map(|v| json!({"required": v})),
        Just(json!({"patternProperties": {"^x": {"type": "string"}}, "additionalProperties": false})),
    ];
    leaf.prop_recursive(4, 48, 4, |inner| {
        prop_oneof![
            prop::collection::btree_map("[a-c]", inner.clone(), 0..3)
                .prop_map(|m| json!({"type": "object", "properties": m})),
            inner.clone().prop_map(|s| json!({"additionalProperties": s})),
            inner.clone().prop_map(|s| json!({"items": s})),
            prop::collection::vec(inner.clone(), 0..3).prop_map(|s| json!({"items": s})),
            prop::collection::vec(inner.clone(), 0..3).prop_map(|s| json!({"prefixItems": s})),
            (prop::sample::select(vec!["anyOf", "oneOf", "allOf"]), prop::collection::vec(inner.clone(), 0..3))
                .prop_map(|(k, s)| json!({k: s})),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| {
                // Merge two schemas' keywords into one.
                let mut out = a.as_object().cloned().unwrap_or_default();
                if let Some(b) = b.as_object() {
                    for (k, v) in b { out.insert(k.clone(), v.clone()); }
                }
                Value::Object(out)
            }),
        ]
    })
}

fn with_defs(body: Value, a: Value, b: Value) -> Value {
    let mut root = body.as_object().cloned().unwrap_or_default();
    root.insert("type".into(), json!("object"));
    root.insert("$defs".into(), json!({"A": a, "B": b}));
    Value::Object(root)
}

proptest! {
    #![proptest_config(cases(16.0))]

    /// The validator and the boot check never panic, whatever the schema and
    /// the value; and a schema the boot check accepts never makes the checker
    /// give up on a `$ref` (a problem no answer could fix).
    #[test]
    fn validator_never_panics(body in arb_schema(), a in arb_schema(), b in arb_schema(), value in arb_json()) {
        let schema = with_defs(body, a, b);
        let problem = output_schema_problem(&schema);
        let problems = validate(&schema, &value);
        prop_assert!(problems.len() <= webcortex_core::schema::MAX_PROBLEMS);
        if problem.is_none() {
            for p in &problems {
                prop_assert!(!p.contains("refers back to itself") && !p.contains("more than 32 $refs"),
                    "boot accepted {} but checking {} said {}", schema, value, p);
            }
        }
    }
}

/// One `$defs` entry of a reference graph.
#[derive(Debug, Clone)]
enum DefKind {
    Alias(usize),
    AllOf(usize),
    Nullable(usize),
    Consumes(usize),
    Terminal,
    IntoAllOf(usize),
    Next,
}

fn defs_graph(kinds: &[DefKind]) -> Value {
    let r = |j: usize| format!("#/$defs/D{}", j % kinds.len());
    let mut defs = Map::new();
    for (i, k) in kinds.iter().enumerate() {
        let def = match k {
            DefKind::Alias(j) => json!({"$ref": r(*j)}),
            DefKind::AllOf(j) => json!({"allOf": [{"$ref": r(*j)}]}),
            DefKind::Nullable(j) => json!({"anyOf": [{"$ref": r(*j)}, {"type": "null"}]}),
            DefKind::Consumes(j) => json!({"type": "object", "properties": {"c": {"$ref": r(*j)}}}),
            DefKind::Terminal => json!({"type": "integer"}),
            DefKind::IntoAllOf(j) => json!({"allOf": [{"$ref": format!("{}/allOf/0", r(*j))}]}),
            DefKind::Next if i + 1 < kinds.len() => json!({"$ref": r(i + 1)}),
            DefKind::Next => json!({"type": "integer"}),
        };
        defs.insert(format!("D{i}"), def);
    }
    json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/D0"}}, "$defs": defs})
}

fn arb_def_kind() -> impl Strategy<Value = DefKind> {
    prop_oneof![
        3 => (0usize..48).prop_map(DefKind::Alias),
        2 => (0usize..48).prop_map(DefKind::AllOf),
        1 => (0usize..48).prop_map(DefKind::Nullable),
        2 => (0usize..48).prop_map(DefKind::Consumes),
        2 => Just(DefKind::Terminal),
        1 => (0usize..48).prop_map(DefKind::IntoAllOf),
        3 => Just(DefKind::Next),
    ]
}

proptest! {
    #![proptest_config(cases(8.0))]

    /// Reference graphs: whatever `$defs` graph the boot check lets through,
    /// checking a value never stops at "refers back to itself" or "more than
    /// 32 $refs": those schemas are the ones boot is documented to refuse.
    #[test]
    fn validator_accepted_ref_graphs_always_resolve(kinds in prop::collection::vec(arb_def_kind(), 1..48), value in arb_json()) {
        let schema = defs_graph(&kinds);
        if output_schema_problem(&schema).is_none() {
            let instance = json!({"x": value});
            for p in validate(&schema, &instance) {
                prop_assert!(!p.contains("refers back to itself") && !p.contains("more than 32 $refs"),
                    "boot accepted the schema but checking said {p}; kinds = {kinds:?}");
            }
        }
    }

    /// Every malformation the 2.9.0 CHANGELOG lists is refused at boot,
    /// wherever in the schema it sits.
    #[test]
    fn validator_listed_malformations_are_refused_at_boot(which in 0usize..18, place in 0usize..8) {
        let bad = [
            json!({"type": "strin"}), json!({"type": 5}), json!({"type": []}),
            json!({"properties": []}), json!({"required": "name"}), json!({"required": [1]}),
            json!({"enum": "a"}), json!({"minItems": -1}), json!({"maxLength": 1.5}),
            json!({"minimum": "3"}), json!({"exclusiveMinimum": true}), json!({"items": [{"type": "string"}]}),
            json!({"anyOf": []}), json!({"oneOf": []}), json!({"allOf": []}),
            json!({"$ref": "#/$defs/Missing"}), json!({"nullable": "yes"}), json!({"uniqueItems": 1}),
        ][which].clone();
        let schema = match place {
            0 => json!({"type": "object", "properties": {"p": bad}}),
            1 => json!({"type": "object", "properties": {"p": {"type": "array", "items": bad}}}),
            2 => json!({"type": "object", "additionalProperties": bad}),
            3 => json!({"type": "object", "properties": {"p": {"anyOf": [{"type": "null"}, bad]}}}),
            4 => json!({"type": "object", "properties": {"p": {"$ref": "#/$defs/D"}}, "$defs": {"D": bad}}),
            5 => json!({"type": "object", "properties": {"p": {"prefixItems": [bad]}}}),
            6 => json!({"type": "object", "allOf": [bad]}),
            _ => json!({"type": "object", "properties": {"p": {"type": "object", "properties": {"q": bad}}}}),
        };
        prop_assert!(output_schema_problem(&schema).is_some(), "boot accepted {}", schema);
    }
}

/// A chain of 40 aliases (`D0 -> D1 -> ... -> D39 -> integer`) has no cycle,
/// so boot accepts it; checking must then follow it rather than refuse every
/// answer at that place.
#[test]
fn validator_long_alias_chain_accepted_at_boot_is_followed() {
    let kinds: Vec<DefKind> = (0..40).map(|i| if i < 39 { DefKind::Next } else { DefKind::Terminal }).collect();
    let schema = defs_graph(&kinds);
    let boot = output_schema_problem(&schema);
    let problems = validate(&schema, &json!({"x": 3}));
    assert!(boot.is_some() || problems.is_empty(), "boot accepted a 40-alias chain, then checking {{\"x\": 3}} said {problems:?}");
}

/// A definition whose `allOf` entry points at itself (`#/$defs/D0/allOf/0`)
/// consumes nothing, so boot should refuse it as it refuses `D0 -> D0`.
#[test]
fn validator_self_pointing_subschema_is_refused_at_boot() {
    let schema = defs_graph(&[DefKind::IntoAllOf(0)]);
    let boot = output_schema_problem(&schema);
    let problems = validate(&schema, &json!({"x": 3}));
    assert!(boot.is_some() || problems.is_empty(), "boot accepted {schema}, then checking {{\"x\": 3}} said {problems:?}");
}

/// `Add | Sub | Lit`: two record types that share recursive field names, the
/// shape of any expression tree or AST an agent might be asked to return.
fn expr_schema() -> Value {
    let binary = |kind: &str| json!({
        "type": "object",
        "properties": {
            "kind": {"const": kind},
            "left": {"$ref": "#/$defs/Expr"},
            "right": {"$ref": "#/$defs/Expr"},
        },
        "required": ["kind", "left", "right"],
        "additionalProperties": false,
    });
    json!({
        "type": "object",
        "properties": {"expr": {"$ref": "#/$defs/Expr"}},
        "required": ["expr"],
        "additionalProperties": false,
        "$defs": {
            "Add": binary("add"),
            "Sub": binary("sub"),
            "Lit": {
                "type": "object",
                "properties": {"kind": {"const": "lit"}, "value": {"type": "integer"}},
                "required": ["kind", "value"],
                "additionalProperties": false,
            },
            "Expr": {"anyOf": [{"$ref": "#/$defs/Add"}, {"$ref": "#/$defs/Sub"}, {"$ref": "#/$defs/Lit"}]},
        },
    })
}

fn expr_chain(depth: usize) -> Value {
    let mut e = json!({"kind": "lit", "value": 0});
    for _ in 0..depth {
        e = json!({"kind": "add", "left": e, "right": {"kind": "lit", "value": 1}});
    }
    json!({"expr": e})
}

/// Checking an answer takes time proportional to the answer, not exponential
/// in its depth. A 14-level expression is about 800 bytes of JSON; with two
/// union members sharing the recursive fields `left`/`right`, every level
/// checks the whole subtree once per member, so 2.9.0 does ~2^depth work
/// (debug build: depth 14 = 1.7 s, 16 = 8 s, 18 = 34 s).
#[test]
fn validator_time_is_not_exponential_in_answer_depth() {
    let schema = expr_schema();
    assert!(output_schema_problem(&schema).is_none());
    let mut last = std::time::Duration::ZERO;
    for depth in [4usize, 8, 10, 12, 14, 40, 100] {
        let value = expr_chain(depth);
        let t = std::time::Instant::now();
        let problems = validate(&schema, &value);
        last = t.elapsed();
        eprintln!("expr depth {depth:>2}: {:>10.3} ms ({} bytes)", last.as_secs_f64() * 1e3, value.to_string().len());
        assert!(problems.is_empty(), "{problems:?}");
    }
    assert!(last < std::time::Duration::from_millis(50), "depth 100 took {last:?}");
}

// ======================================================================
// Templates and the json filter
// ======================================================================

fn templates() -> &'static Templates {
    static T: OnceLock<Templates> = OnceLock::new();
    T.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("wc-proptest-templates-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let files = [
            ("page.html", "<script>const d = {{ data | json }};</script>\n<p>{{ text }}</p>\n<div data-x='{{ data | tojson }}'></div>\n<pre>{{ data | json(indent=2) }}</pre>"),
            ("page.json", "{\"q\": {{ text }}, \"d\": {{ data }}, \"j\": {{ data | json }}}"),
            ("page.txt", "{{ text }}"),
        ];
        for (name, body) in files {
            std::fs::write(dir.join(name), body).expect("write template");
        }
        let names = ["a.HTML", "b.Htm.J2", "c.xml.jinja", "d.svg.JINJA2", "e.json", "f.JSON.j2", "g.txt", "h.css",
                     "i.js", "j.mjs", "k.csv", "l.md", "m", "n.j2", "o.html.txt", "p.txt.html", "q.XHTML", "r.Html.Jinja"];
        for name in names {
            std::fs::write(dir.join(name), "{{ text }}").expect("write template");
        }
        Templates::load(&TemplateConfig { dir: dir.to_string_lossy().into_owned(), autoescape: true }).expect("load")
    })
}

fn render(name: &str, ctx: &Value) -> (String, String, u16) {
    let res: WebCortexResponse = templates().render(name, ctx, 200);
    let ct = res.headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.clone()).unwrap_or_default();
    (String::from_utf8(res.body.to_vec()).expect("utf-8 page"), ct, res.status)
}

fn html_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&#x2f;", "/")
        .replace("&#x2F;", "/")
        .replace("&#47;", "/")
        .replace("&amp;", "&")
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
    let i = s.find(start).expect("start marker") + start.len();
    let j = s[i..].find(end).expect("end marker") + i;
    &s[i..j]
}

proptest! {
    #![proptest_config(cases(8.0))]

    /// `{{ data | json }}` inside `<script>` and a single-quoted attribute is
    /// JSON that parses back to the data and holds none of `<`, `>`, `&`, `'`,
    /// U+2028 or U+2029; `{{ text }}` in HTML holds no raw markup character and
    /// unescapes to the text.
    #[test]
    fn template_json_filter_is_script_safe_and_round_trips(data in arb_json(), text in arb_text()) {
        let (page, ct, status) = render("page.html", &json!({"data": data, "text": text}));
        prop_assert_eq!(status, 200, "{}", page);
        prop_assert!(ct.starts_with("text/html"));
        for (start, end) in [("const d = ", ";</script>"), ("data-x='", "'></div>"), ("<pre>", "</pre>")] {
            let js = between(&page, start, end);
            for bad in ['<', '>', '&', '\'', '\u{2028}', '\u{2029}'] {
                prop_assert!(!js.contains(bad), "{:?} raw in {}", bad, js);
            }
            let back: Value = serde_json::from_str(js).map_err(|e| TestCaseError::fail(format!("{e}: {js}")))?;
            prop_assert!(oracle_eq(&back, &data), "{} came back as {}", data, back);
        }
        let p = between(&page, "<p>", "</p>");
        for bad in ['<', '>', '"', '\''] {
            prop_assert!(!p.contains(bad), "{:?} raw in <p>{}</p>", bad, p);
        }
        prop_assert_eq!(html_unescape(p), text.clone());
    }

    /// A `.json` page writes every value as JSON, so the page is JSON and a
    /// value cannot break out of its place.
    #[test]
    fn template_json_page_is_json(data in arb_json(), text in arb_text()) {
        let (page, ct, status) = render("page.json", &json!({"data": data, "text": text}));
        prop_assert_eq!(status, 200, "{}", page);
        prop_assert_eq!(ct, "application/json");
        let v: Value = serde_json::from_str(&page).map_err(|e| TestCaseError::fail(format!("{e}: {page}")))?;
        prop_assert_eq!(v.as_object().map(|o| o.len()), Some(3), "{}", page);
        prop_assert_eq!(&v["q"], &Value::String(text));
        prop_assert!(oracle_eq(&v["d"], &data), "{} vs {}", v["d"], data);
        prop_assert!(oracle_eq(&v["j"], &data), "{} vs {}", v["j"], data);
    }
}

/// The content type and the escaping come from one judgement, so a page
/// served as markup is always escaped and an unescaped page is never markup.
#[test]
fn template_content_type_and_escaping_agree() {
    let text = "<b x=\"1\">'&'</b>";
    for name in ["a.HTML", "b.Htm.J2", "c.xml.jinja", "d.svg.JINJA2", "e.json", "f.JSON.j2", "g.txt", "h.css",
                 "i.js", "j.mjs", "k.csv", "l.md", "m", "n.j2", "o.html.txt", "p.txt.html", "q.XHTML", "r.Html.Jinja", "page.txt"] {
        let (page, ct, status) = render(name, &json!({"text": text}));
        assert_eq!(status, 200, "{name}");
        let markup = ["text/html", "application/xml", "image/svg+xml", "application/xhtml"].iter().any(|m| ct.starts_with(m));
        if markup {
            assert!(!page.contains('<'), "{name} served as {ct} but not escaped: {page}");
        } else if ct == "application/json" {
            assert_eq!(serde_json::from_str::<Value>(&page).unwrap(), json!(text), "{name}");
        } else {
            assert_eq!(page, text, "{name} ({ct})");
            assert!(!ct.contains("html") && !ct.contains("xml") && !ct.contains("svg"), "{name}: {ct}");
        }
    }
}

// ======================================================================
// The error catalogue
// ======================================================================

#[test]
fn every_catalogued_code_is_complete() {
    let mut seen = std::collections::HashSet::new();
    let categories = ["internal", "config", "http", "agent", "limits", "devices", "auth"];
    for e in errors::CATALOGUE {
        assert!(seen.insert(e.code), "{} twice", e.code);
        assert!(e.code.len() == 5 && e.code.starts_with("WC") && e.code[2..].bytes().all(|b| b.is_ascii_digit()), "{}", e.code);
        assert!(!e.title.trim().is_empty(), "{} has no title", e.code);
        assert!(e.hint.trim().len() >= 20, "{} has no real hint: {:?}", e.code, e.hint);
        assert!(categories.contains(&e.category), "{}: {}", e.code, e.category);
        if let Some(s) = e.status {
            assert!((400..=599).contains(&s), "{} has status {s}", e.code);
        }
        assert_eq!(errors::lookup(e.code).map(|x| x.code), Some(e.code));
        assert_eq!(errors::code_in(&format!("failed: {} (see docs)", e.code)), Some(e.code));
    }
}

proptest! {
    #![proptest_config(cases(16.0))]

    /// Every status gets a catalogued code with a hint, and the error body
    /// carries status, message, code and hint.
    #[test]
    fn every_error_response_carries_code_and_hint(status in 100u16..1000, message in "\\PC{0,40}") {
        let res = WebCortexResponse::error(status, message.clone());
        let body = res.json_value();
        let code = body["error"]["code"].as_str().unwrap_or_default().to_string();
        prop_assert!(errors::lookup(&code).is_some(), "status {} got {}", status, code);
        prop_assert!(!body["error"]["hint"].as_str().unwrap_or_default().is_empty());
        prop_assert_eq!(&body["error"]["message"], &Value::String(message));
        prop_assert_eq!(body["error"]["status"].as_u64(), Some(status as u64));
        prop_assert_eq!(res.error_code(), Some(code));
    }

    /// `code_in` finds the first catalogued code that stands alone, and never
    /// panics on any text; `classify_run_error` always answers a catalogued code.
    #[test]
    fn code_in_finds_standalone_codes(prefix in "\\PC{0,12}", suffix in "\\PC{0,12}", idx in any::<prop::sample::Index>(), glue in prop::sample::select(vec!["", " ", "x", "9", ":", "é"])) {
        let e = idx.get(errors::CATALOGUE);
        let text = format!("{prefix}{glue}{}{glue}{suffix}", e.code);
        let found = errors::code_in(&text);
        if let Some(c) = found {
            prop_assert!(errors::lookup(c).is_some());
            prop_assert!(text.contains(c));
        }
        let before = format!("{prefix}{glue}");
        let after = format!("{glue}{suffix}");
        let standalone = !before.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.chars().next().is_some_and(|c| c.is_ascii_digit());
        if standalone && errors::code_in(&before).is_none() {
            prop_assert_eq!(found, Some(e.code), "in {:?}", text);
        }
        let code = errors::classify_run_error(&text);
        prop_assert!(errors::lookup(code).is_some());
    }
}

// ======================================================================
// OTEL_EXPORTER_OTLP_HEADERS
// ======================================================================

fn tracer() -> &'static Tracer {
    static T: OnceLock<Tracer> = OnceLock::new();
    T.get_or_init(|| {
        let cfg: TracingConfig = serde_json::from_value(json!({
            "enabled": true, "otlp_endpoint": "http://collector:4318", "otlp_headers_env": "WC_PROP_HDRS",
        }))
        .expect("tracing config");
        Tracer::new(&cfg)
    })
}

fn otlp_headers(raw: &str) -> (Vec<(String, String)>, Vec<String>) {
    let (target, warnings) = tracer().export_plan(|k| (k == "WC_PROP_HDRS").then(|| raw.to_string()));
    (target.expect("export target").headers, warnings)
}

proptest! {
    #![proptest_config(cases(16.0))]

    /// Percent-encoded `k=v` pairs, padded with spaces, come back as sent; a
    /// pair that cannot be a header is skipped with a warning that names its
    /// position and never its value.
    #[test]
    fn otlp_headers_round_trip(pairs in prop::collection::vec(("[A-Za-z0-9!#$&'*+.^_`|~-]{1,10}", prop_oneof![
            4 => "[ -~]{0,16}",
            1 => "\\PC{0,8}",
        ]), 0..6), pad in prop::collection::vec(prop::sample::select(vec!["", " ", "  ", "\t"]), 2)) {
        let raw: Vec<String> = pairs.iter().map(|(k, v)| {
            format!("{}{}={}{}", pad[0],
                percent_encoding::utf8_percent_encode(k, UNRESERVED),
                percent_encoding::utf8_percent_encode(&format!("\u{1}Ω{v}"), UNRESERVED), pad[1])
        }).collect();
        let (headers, warnings) = otlp_headers(&raw.join(","));
        let want: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.clone(), format!("\u{1}Ω{v}"))).collect();
        // A control character (U+0001) cannot be in a header value, so every
        // pair is skipped with a warning, which must not show the value.
        prop_assert!(headers.is_empty(), "{:?}", headers);
        prop_assert_eq!(warnings.len(), want.len());
        for w in &warnings {
            prop_assert!(!w.contains('Ω'), "a warning showed a value: {}", w);
        }
        // Without the control character, every pair comes back exactly (a
        // header value may carry obs-text, so non-ASCII text is kept).
        let raw: Vec<String> = pairs.iter().map(|(k, v)| {
            format!("{}{}={}{}", pad[0],
                percent_encoding::utf8_percent_encode(k, UNRESERVED),
                percent_encoding::utf8_percent_encode(v, UNRESERVED), pad[1])
        }).collect();
        let want = pairs.clone();
        let (headers, warnings) = otlp_headers(&raw.join(","));
        prop_assert!(warnings.is_empty(), "{:?}", warnings);
        prop_assert_eq!(headers, want);
    }

    /// Garbage never panics and never leaks into a warning.
    #[test]
    fn otlp_headers_garbage_never_leaks(raw in "[Ω%=, a-zA-Z0-9\t\u{0}\r\n]{0,40}") {
        let (_, warnings) = otlp_headers(&raw);
        for w in &warnings {
            prop_assert!(!w.contains('Ω'), "a warning showed input: {}", w);
        }
    }
}

// ======================================================================
// Query strings
// ======================================================================

proptest! {
    #![proptest_config(cases(16.0))]

    /// Form-encoded pairs, repeats and order included, decode to what was sent.
    #[test]
    fn query_pairs_round_trip(pairs in prop::collection::vec(("\\PC{0,8}", "\\PC{0,8}"), 0..6)) {
        let enc = |s: &str| percent_encoding::utf8_percent_encode(s, UNRESERVED).to_string();
        let raw: Vec<String> = pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect();
        let got = parse_query_pairs(&raw.join("&"));
        prop_assert_eq!(got, pairs);
    }
}

// ======================================================================
// JWT acceptance
// ======================================================================

const SECRET: &str = "a-proptest-secret-that-is-long-enough-0123456789";

fn authenticator(audience: bool, issuer: bool, leeway: u64) -> Authenticator {
    static VAR: OnceLock<String> = OnceLock::new();
    let var = VAR.get_or_init(|| {
        let v = format!("WC_PROPTEST_JWT_{}", std::process::id());
        // Set once, before any authenticator reads it.
        unsafe { std::env::set_var(&v, SECRET) };
        v
    });
    let cfg: AuthConfig = serde_json::from_value(json!({
        "api_keys": {"WC_PROPTEST_UNSET_KEY": {"id": "ops", "scopes": ["admin"]}},
        "jwt": {
            "secret_env": var, "algorithm": "HS256",
            "audience": if audience { json!("api") } else { Value::Null },
            "issuer": if issuer { json!("https://idp") } else { Value::Null },
            "leeway_secs": leeway,
        },
    }))
    .expect("auth config");
    Authenticator::build(&cfg).expect("authenticator")
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

#[derive(Debug, Clone)]
enum Time {
    Absent,
    Offset(i64),
    Float(i64),
    Text,
    Null,
}

fn arb_time() -> impl Strategy<Value = Time> {
    prop_oneof![
        Just(Time::Absent),
        prop::sample::select(vec![-3600i64, -600, -15, 15, 600, 3600]).prop_map(Time::Offset),
        prop::sample::select(vec![-600i64, 600]).prop_map(Time::Float),
        Just(Time::Text),
        Just(Time::Null),
    ]
}

fn time_claim(t: &Time) -> Option<Value> {
    match t {
        Time::Absent => None,
        Time::Offset(o) => Some(json!(now() + o)),
        Time::Float(o) => Some(json!((now() + o) as f64 + 0.5)),
        Time::Text => Some(json!("tomorrow")),
        Time::Null => Some(Value::Null),
    }
}

proptest! {
    #![proptest_config(cases(4.0))]

    /// Whatever the claims, a token is never accepted when the docs say it is
    /// refused: a configured `aud`/`iss` must be present and match, `exp` must
    /// be present and not past (plus leeway), `nbf` not in the future (minus
    /// leeway), and the subject may not be an API key's id or `watcher:*`.
    /// What is accepted carries the token's subject and its real expiry, and
    /// none of the delegation claims a token may not write.
    #[test]
    fn jwt_never_accepts_what_the_docs_refuse(
        want_aud in any::<bool>(), want_iss in any::<bool>(), leeway in prop::sample::select(vec![0u64, 30]),
        sub in prop::sample::select(vec![None, Some(json!("")), Some(json!("ops")), Some(json!("watcher:door")), Some(json!("watcher:")),
            Some(json!("Ops")), Some(json!("u1")), Some(json!("jwt-subject-unset")), Some(json!(5)), Some(json!(["u1"]))]),
        aud in prop::sample::select(vec![None, Some(json!("api")), Some(json!("other")), Some(json!(["x", "api"])), Some(json!(["x"])),
            Some(json!([])), Some(json!(5)), Some(Value::Null)]),
        iss in prop::sample::select(vec![None, Some(json!("https://idp")), Some(json!("https://idp/")), Some(json!("other")), Some(json!(5)), Some(Value::Null)]),
        exp in arb_time(), nbf in arb_time(),
        reserved in any::<bool>(),
    ) {
        let a = authenticator(want_aud, want_iss, leeway);
        let mut claims = Map::new();
        if let Some(s) = &sub { claims.insert("sub".into(), s.clone()); }
        if let Some(v) = &aud { claims.insert("aud".into(), v.clone()); }
        if let Some(v) = &iss { claims.insert("iss".into(), v.clone()); }
        if let Some(v) = time_claim(&exp) { claims.insert("exp".into(), v); }
        if let Some(v) = time_claim(&nbf) { claims.insert("nbf".into(), v); }
        if reserved {
            claims.insert("root".into(), json!("alice"));
            claims.insert("root_anonymous".into(), json!(false));
            claims.insert("token_expires_at".into(), json!(99_999_999_999u64));
        }
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &Value::Object(claims.clone()),
            &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
        ).expect("sign");
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_string(), format!("Bearer {token}"));
        let result = a.authenticate(&headers);

        let l = leeway as i64;
        // By value: RFC 7519 NumericDates may be fractional. A malformed
        // `exp` must be refused (it is required); a malformed `nbf` names no
        // time, so either answer is defensible and only its value is checked.
        let exp_ok = matches!(exp, Time::Offset(o) | Time::Float(o) if o + l >= 0);
        let nbf_ok = match nbf { Time::Offset(o) | Time::Float(o) => o - l <= 0, _ => true };
        let nbf_plain = matches!(nbf, Time::Absent | Time::Offset(_));
        let aud_ok = !want_aud || matches!(&aud, Some(Value::String(s)) if s == "api")
            || matches!(&aud, Some(Value::Array(xs)) if xs.iter().any(|x| x == "api"));
        let iss_ok = !want_iss || matches!(&iss, Some(Value::String(s)) if s == "https://idp");
        let sub_ok = match &sub {
            Some(Value::String(s)) => s != "ops" && !s.starts_with("watcher:"),
            None => true,
            _ => false,
        };
        if let Ok(p) = &result {
            prop_assert!(exp_ok, "accepted with exp {:?} (leeway {}): {:?}", exp, leeway, claims);
            prop_assert!(nbf_ok, "accepted with nbf {:?} (leeway {}): {:?}", nbf, leeway, claims);
            prop_assert!(aud_ok, "accepted with aud {:?} when 'api' is required", aud);
            prop_assert!(iss_ok, "accepted with iss {:?} when 'https://idp' is required", iss);
            prop_assert!(sub_ok, "accepted subject {:?}", sub);
            let want_id = match &sub { Some(Value::String(s)) if !s.is_empty() => s.clone(), _ => JWT_SUBJECT_UNSET.to_string() };
            prop_assert_eq!(&p.id, &want_id);
            prop_assert_eq!(p.root_id(), want_id.as_str());
            for name in ["root", "root_anonymous", "agent", "on_behalf_of"] {
                prop_assert!(!p.claims.contains_key(name), "{} survived", name);
            }
            let real_exp = claims.get("exp").and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)));
            prop_assert_eq!(p.token_expires_at(), real_exp);
        }
        // Liveness: a token that meets every rule is accepted, whatever else
        // it carries (aud given while none is configured is the library's call).
        let aud_extra = !want_aud && aud.is_some();
        let iss_typed = iss.as_ref().is_none_or(Value::is_string);
        let exp_plain = matches!(exp, Time::Offset(_));
        if exp_ok && exp_plain && nbf_ok && nbf_plain && aud_ok && iss_ok && sub_ok && !aud_extra && iss_typed {
            prop_assert!(result.is_ok(), "refused a good token: {:?} -> {:?}", claims, result.err());
        }
    }
}
