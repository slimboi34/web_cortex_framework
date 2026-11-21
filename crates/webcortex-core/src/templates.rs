//! Server-side rendering, executed in Rust.
//!
//! **The constraint that keeps this clean:** a template receives a data object
//! and nothing else. It has no database handle, no way to call Python, no
//! ability to issue a query. Django's template layer grew unmaintainable
//! precisely because template tags could reach back into application code and
//! trigger work; here that is structurally impossible, so a template can only
//! ever be presentation.
//!
//! Data comes from one of three declared sources — constant, SQL, or a Python
//! handler returning a dict — resolved *before* rendering begins.

use crate::http::WebCortexResponse;
use crate::manifest::TemplateConfig;
use minijinja::Environment;
use serde_json::Value;

pub struct Templates {
    env: Environment<'static>,
}

impl Templates {
    pub fn load(cfg: &TemplateConfig) -> Result<Self, String> {
        let dir = std::path::Path::new(&cfg.dir);
        if !dir.is_dir() {
            return Err(format!(
                "template directory {:?} does not exist",
                cfg.dir
            ));
        }

        let mut env = Environment::new();
        env.set_loader(minijinja::path_loader(&cfg.dir));
        // Autoescape by default and derive it from the file extension: an
        // .html template escapes HTML, a .json one writes every value as JSON,
        // and a .txt one is plain text — served as text/plain, so a browser
        // never renders what it holds as markup. XSS-by-default is not an
        // acceptable trade for a framework in 2026.
        if cfg.autoescape {
            env.set_auto_escape_callback(|name| match kind(name) {
                Kind::Markup(_) => minijinja::AutoEscape::Html,
                Kind::Json => minijinja::AutoEscape::Json,
                Kind::Other(_) => minijinja::AutoEscape::None,
            });
        } else {
            env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
        }

        add_filters(&mut env);
        Ok(Self { env })
    }

    /// Fail fast: confirm every declared template parses at boot rather than
    /// discovering a syntax error when a user hits the page.
    pub fn verify(&self, names: &[String]) -> Result<(), String> {
        for name in names {
            self.env
                .get_template(name)
                .map_err(|e| format!("template {name:?} failed to load: {e}"))?;
        }
        Ok(())
    }

    pub fn render(&self, name: &str, context: &Value, status: u16) -> WebCortexResponse {
        let tmpl = match self.env.get_template(name) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(template = %name, error = %e, "template not found");
                return WebCortexResponse::coded(500, "WC004", format!("template {name:?} is unavailable"));
            }
        };

        match tmpl.render(context) {
            Ok(text) => WebCortexResponse {
                status,
                headers: vec![("content-type".into(), content_type(name).into())],
                body: bytes::Bytes::from(text),
            },
            Err(e) => {
                // Template errors carry source snippets; those belong in the log,
                // never in a response body where they leak app internals.
                tracing::error!(template = %name, error = %e, "template render failed");
                WebCortexResponse::coded(500, "WC004", format!("failed to render {name:?}"))
            }
        }
    }
}

/// What a template produces, judged by its extension: case-insensitive, and
/// looking through a trailing Jinja suffix (`page.html.j2`, `feed.XML.jinja`).
/// The same judgement sets both the escaping and the content type, so the two
/// cannot disagree: in 2.8 a `.txt` or `.json` template was not escaped but
/// was still served as `text/html`, so a value holding `<script>` ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// HTML, XML or SVG: HTML-escaped. Carries the content type.
    Markup(&'static str),
    /// JSON: every value is written as JSON.
    Json,
    /// Anything else: not escaped, and never served as markup.
    Other(&'static str),
}

const TEXT: &str = "text/plain; charset=utf-8";

fn kind(name: &str) -> Kind {
    let lower = name.to_ascii_lowercase();
    let inner = [".j2", ".jinja", ".jinja2"]
        .iter()
        .find_map(|suffix| lower.strip_suffix(suffix))
        .unwrap_or(&lower);
    let ext = inner.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    // A bare name ("html", "a.j2") has no extension and is plain text.
    let ext = if inner.contains('.') { ext } else { "" };
    match ext {
        "html" | "htm" | "xhtml" => Kind::Markup("text/html; charset=utf-8"),
        "xml" => Kind::Markup("application/xml"),
        "svg" => Kind::Markup("image/svg+xml"),
        "json" => Kind::Json,
        "css" => Kind::Other("text/css; charset=utf-8"),
        "js" | "mjs" => Kind::Other("text/javascript; charset=utf-8"),
        "csv" => Kind::Other("text/csv; charset=utf-8"),
        "md" => Kind::Other("text/markdown; charset=utf-8"),
        _ => Kind::Other(TEXT),
    }
}

/// Whether a template's output is markup, so HTML-escaped.
#[cfg(test)]
fn escapes_html(name: &str) -> bool {
    matches!(kind(name), Kind::Markup(_))
}

/// The `content-type` a rendered page is served with.
fn content_type(name: &str) -> &'static str {
    match kind(name) {
        Kind::Markup(ct) | Kind::Other(ct) => ct,
        Kind::Json => "application/json",
    }
}

/// `json` (and its alias `tojson`): the value as JSON that is safe to place
/// inside a `<script>` element or a single-quoted attribute of an HTML page,
/// and is still valid JSON. `<`, `>`, `&` and `'` are written as `\uXXXX`
/// escapes, so `</script>` or `<!--` in the data cannot end the element, and
/// so are U+2028 and U+2029, which older JavaScript reads as line breaks. The
/// result is marked safe, so HTML autoescaping does not turn its quotes into
/// `&quot;` (which made the documented `const d = {{ data | json }}` a syntax
/// error in 2.8). An optional indent (a number, or `true` for 2) pretty-prints.
fn json_filter(
    value: &minijinja::Value,
    indent: Option<minijinja::Value>,
    args: minijinja::value::Kwargs,
) -> Result<minijinja::Value, minijinja::Error> {
    let indent = match indent {
        Some(indent) => Some(indent),
        None => args.get("indent")?,
    };
    args.assert_all_used()?;
    let indent = match indent {
        None => None,
        Some(v) => match bool::try_from(v.clone()) {
            Ok(true) => Some(2),
            Ok(false) => None,
            Err(_) => Some(usize::try_from(v)?),
        },
    };
    let text = match indent {
        None => serde_json::to_string(value),
        Some(n) => {
            let mut out = Vec::new();
            let pad = " ".repeat(n.min(16));
            let formatter = serde_json::ser::PrettyFormatter::with_indent(pad.as_bytes());
            let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
            serde::Serialize::serialize(value, &mut ser).map(|()| String::from_utf8_lossy(&out).into_owned())
        }
    }
    .map_err(|e| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, "cannot serialize to JSON").with_source(e))?;
    Ok(minijinja::Value::from_safe_string(script_safe(&text)))
}

/// Escape the characters of JSON text that HTML or old JavaScript would read
/// as something else. Each can only occur inside a JSON string, where a
/// `\uXXXX` escape means the same character, so the JSON is unchanged.
fn script_safe(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\'' => out.push_str("\\u0027"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

fn add_filters(env: &mut Environment<'static>) {
    // A small, deliberately boring filter set. Anything more expressive belongs
    // in the data source, not the template.
    env.add_filter("json", json_filter);
    env.add_filter("tojson", json_filter);
    env.add_filter("currency", |v: f64| format!("{v:.2}"));
    env.add_filter("truncate_words", |s: String, n: usize| {
        let words: Vec<&str> = s.split_whitespace().collect();
        if words.len() <= n {
            s
        } else {
            format!("{}…", words[..n].join(" "))
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_templates(files: &[(&str, &str)]) -> (tempdir::TempDirLike, Templates) {
        let dir = tempdir::TempDirLike::new();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).expect("write template");
        }
        let t = Templates::load(&TemplateConfig {
            dir: dir.path().to_string_lossy().into_owned(),
            autoescape: true,
        })
        .expect("templates load");
        (dir, t)
    }

    // Minimal temp-dir helper; avoids a dev-dependency for three tests.
    mod tempdir {
        pub struct TempDirLike(std::path::PathBuf);
        impl TempDirLike {
            pub fn new() -> Self {
                let base = std::env::temp_dir().join(format!(
                    "webcortex-tmpl-{}",
                    uuid::Uuid::new_v4()
                ));
                std::fs::create_dir_all(&base).expect("create temp dir");
                Self(base)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDirLike {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn renders_with_context() {
        let (_d, t) = temp_templates(&[("hi.html", "<h1>{{ data.name }}</h1>")]);
        let res = t.render("hi.html", &serde_json::json!({"data": {"name": "Ada"}}), 200);
        assert_eq!(res.status, 200);
        assert_eq!(String::from_utf8_lossy(&res.body), "<h1>Ada</h1>");
    }

    #[test]
    fn html_is_escaped_by_default() {
        let (_d, t) = temp_templates(&[("x.html", "{{ data }}")]);
        let res = t.render("x.html", &serde_json::json!({"data": "<script>alert(1)</script>"}), 200);
        let body = String::from_utf8_lossy(&res.body);
        assert!(!body.contains("<script>"), "XSS must be escaped: {body}");
        assert!(body.contains("&lt;script&gt;"));
    }

    #[test]
    fn markup_is_recognised_through_jinja_suffixes_and_case() {
        for name in ["a.html", "a.HTML", "a.htm", "a.html.j2", "a.html.jinja", "a.Html.Jinja2",
                     "a.xml", "a.svg", "a.xhtml"] {
            assert!(escapes_html(name), "{name} must escape");
        }
        for name in ["a.txt", "a.json", "a.j2", "a.txt.jinja", "html", "a.md"] {
            assert!(!escapes_html(name), "{name} must not escape");
        }
        let (_d, t) = temp_templates(&[("x.html.j2", "{{ data }}")]);
        let res = t.render("x.html.j2", &serde_json::json!({"data": "<script>"}), 200);
        assert_eq!(String::from_utf8_lossy(&res.body), "&lt;script&gt;");
    }

    #[test]
    fn non_html_extensions_are_not_escaped() {
        let (_d, t) = temp_templates(&[("x.txt", "{{ data }}")]);
        let res = t.render("x.txt", &serde_json::json!({"data": "a<b"}), 200);
        assert_eq!(String::from_utf8_lossy(&res.body), "a<b");
    }

    fn content_type_of(res: &WebCortexResponse) -> &str {
        res.headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.as_str()).unwrap_or("")
    }

    #[test]
    fn the_content_type_follows_the_extension_like_the_escaping_does() {
        for (name, ct) in [
            ("a.html", "text/html; charset=utf-8"), ("a.HTM", "text/html; charset=utf-8"),
            ("a.xhtml.j2", "text/html; charset=utf-8"), ("a.html.jinja2", "text/html; charset=utf-8"),
            ("feed.XML", "application/xml"), ("i.svg.jinja", "image/svg+xml"),
            ("d.json", "application/json"), ("d.JSON.j2", "application/json"),
            ("n.txt", "text/plain; charset=utf-8"), ("n.txt.jinja", "text/plain; charset=utf-8"),
            ("s.css", "text/css; charset=utf-8"), ("a.j2", "text/plain; charset=utf-8"),
            ("html", "text/plain; charset=utf-8"), ("notes.md", "text/markdown; charset=utf-8"),
            ("x.unknown", "text/plain; charset=utf-8"),
        ] {
            assert_eq!(content_type(name), ct, "{name}");
        }
        // A .txt page holding markup is plain text, not a page that runs it.
        let (_d, t) = temp_templates(&[("echo.txt", "hello {{ q }}")]);
        let res = t.render("echo.txt", &serde_json::json!({"q": "<script>alert(1)</script>"}), 200);
        assert_eq!(content_type_of(&res), "text/plain; charset=utf-8");
        assert_eq!(String::from_utf8_lossy(&res.body), "hello <script>alert(1)</script>");
        let (_d, t) = temp_templates(&[("p.html", "x")]);
        assert_eq!(content_type_of(&t.render("p.html", &serde_json::json!({}), 200)), "text/html; charset=utf-8");
    }

    #[test]
    fn a_json_template_writes_values_as_json() {
        let (_d, t) = temp_templates(&[("echo.json", "{\"q\": {{ q }}, \"n\": {{ n }}, \"raw\": {{ q | safe }}}")]);
        let res = t.render("echo.json", &serde_json::json!({"q": "x\", \"admin\": true, \"y\": \"<b>", "n": 3}), 200);
        assert_eq!(content_type_of(&res), "application/json");
        let body = String::from_utf8_lossy(&res.body).into_owned();
        assert!(body.starts_with("{\"q\": \"x\\\", \\\"admin\\\""), "{body}");
        let (_d, t) = temp_templates(&[("ok.json", "{\"q\": {{ q }}, \"d\": {{ d | json }}}")]);
        let res = t.render("ok.json", &serde_json::json!({"q": "x\", \"admin\": true", "d": {"a": [1, "<"]}}), 200);
        let parsed: serde_json::Value = serde_json::from_slice(&res.body).expect("valid JSON");
        assert_eq!(parsed, serde_json::json!({"q": "x\", \"admin\": true", "d": {"a": [1, "<"]}}));
    }

    #[test]
    fn the_json_filter_is_script_safe_valid_json_and_not_html_escaped() {
        let (_d, t) = temp_templates(&[
            ("s.html", "<script>const d = {{ data | json }}</script>"),
            ("t.html", "<script>const d = {{ data | tojson }}</script>"),
            ("u.html", "<script>const d = {{ data | json | safe }}</script>"),
            ("a.html", "<div data-x='{{ data | json }}'></div>"),
            ("i.html", "{{ data | tojson(indent=2) }}"),
        ]);
        let payload = "</script><script>alert(document.cookie)</script><!-- ' & \u{2028}";
        let ctx = serde_json::json!({"data": {"name": payload, "n": 1}});
        for name in ["s.html", "t.html", "u.html"] {
            let body = String::from_utf8_lossy(&t.render(name, &ctx, 200).body).into_owned();
            let inner = body.strip_prefix("<script>const d = ").and_then(|b| b.strip_suffix("</script>")).expect(&body);
            assert!(!inner.contains('<') && !inner.contains('>') && !inner.contains('&') && !inner.contains('\''), "{name}: {body}");
            assert!(!inner.contains("&quot;"), "{name}: {body}");
            assert!(!inner.contains('\u{2028}'), "{name}: {body}");
            let parsed: serde_json::Value = serde_json::from_str(inner).expect("valid JSON");
            assert_eq!(parsed, ctx["data"], "{name}");
        }
        let attr = String::from_utf8_lossy(&t.render("a.html", &ctx, 200).body).into_owned();
        assert_eq!(attr.matches('\'').count(), 2, "only the attribute's own quotes: {attr}");
        assert!(!attr.contains("<script>") && !attr.contains("<!--"), "{attr}");
        let pretty = String::from_utf8_lossy(&t.render("i.html", &ctx, 200).body).into_owned();
        assert!(pretty.contains("\n  \""), "{pretty}");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&pretty).unwrap(), ctx["data"]);
    }

    #[test]
    fn a_missing_template_is_a_500_not_a_panic() {
        let (_d, t) = temp_templates(&[("a.html", "x")]);
        assert_eq!(t.render("nope.html", &serde_json::json!({}), 200).status, 500);
    }

    #[test]
    fn render_errors_do_not_leak_template_source() {
        let (_d, t) = temp_templates(&[("bad.html", "{{ data.missing.deeper }}")]);
        let res = t.render("bad.html", &serde_json::json!({"data": {}}), 200);
        let body = String::from_utf8_lossy(&res.body);
        assert_eq!(res.status, 500);
        assert!(!body.contains("data.missing"), "leaked internals: {body}");
    }

    #[test]
    fn verify_catches_syntax_errors_at_boot() {
        let (_d, t) = temp_templates(&[("broken.html", "{% if %}")]);
        assert!(t.verify(&["broken.html".into()]).is_err());
    }

    #[test]
    fn inheritance_works() {
        let (_d, t) = temp_templates(&[
            ("base.html", "<body>{% block main %}{% endblock %}</body>"),
            ("page.html", "{% extends 'base.html' %}{% block main %}hi{% endblock %}"),
        ]);
        let res = t.render("page.html", &serde_json::json!({}), 200);
        assert_eq!(String::from_utf8_lossy(&res.body), "<body>hi</body>");
    }
}
