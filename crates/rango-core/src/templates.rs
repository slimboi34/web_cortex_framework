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

use crate::http::RangoResponse;
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
        // Autoescape by default and derive it from the file extension, so an
        // .html template escapes and a .txt or .json one does not. XSS-by-
        // default is not an acceptable trade for a framework in 2026.
        if cfg.autoescape {
            env.set_auto_escape_callback(|name| {
                if name.ends_with(".html") || name.ends_with(".htm") || name.ends_with(".xml") {
                    minijinja::AutoEscape::Html
                } else {
                    minijinja::AutoEscape::None
                }
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

    pub fn render(&self, name: &str, context: &Value, status: u16) -> RangoResponse {
        let tmpl = match self.env.get_template(name) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(template = %name, error = %e, "template not found");
                return RangoResponse::error(500, format!("template {name:?} is unavailable"));
            }
        };

        match tmpl.render(context) {
            Ok(html) => RangoResponse {
                status,
                headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
                body: bytes::Bytes::from(html),
            },
            Err(e) => {
                // Template errors carry source snippets; those belong in the log,
                // never in a response body where they leak app internals.
                tracing::error!(template = %name, error = %e, "template render failed");
                RangoResponse::error(500, format!("failed to render {name:?}"))
            }
        }
    }
}

fn add_filters(env: &mut Environment<'static>) {
    // A small, deliberately boring filter set. Anything more expressive belongs
    // in the data source, not the template.
    env.add_filter("json", |v: minijinja::Value| {
        serde_json::to_string(&v).unwrap_or_else(|_| "null".into())
    });
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
                    "rango-tmpl-{}",
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
    fn non_html_extensions_are_not_escaped() {
        let (_d, t) = temp_templates(&[("x.txt", "{{ data }}")]);
        let res = t.render("x.txt", &serde_json::json!({"data": "a<b"}), 200);
        assert_eq!(String::from_utf8_lossy(&res.body), "a<b");
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
