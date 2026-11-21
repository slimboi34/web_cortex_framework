//! Static file serving.
//!
//! Small, but the traversal defence has to be exactly right: this is the one op
//! that turns a request string into a filesystem path, and it is the single most
//! commonly exploited feature in any web framework.

use crate::http::WebCortexResponse;
use std::path::{Component, Path, PathBuf};

pub struct FileServer {
    root: PathBuf,
    index: Option<String>,
    cache_secs: u64,
}

/// What a request for a static file comes to: an answer with no file behind
/// it (a redirect, a 304, a 404), or a file to send after its headers.
pub enum FileReply {
    Done(WebCortexResponse),
    /// The response head (200, its headers, an empty body), the open file,
    /// and how many bytes of it to send.
    Stream { head: WebCortexResponse, file: std::fs::File, len: u64 },
}

impl FileServer {
    pub fn new(dir: &str, index: Option<String>, cache_secs: u64) -> Result<Self, String> {
        let root = std::fs::canonicalize(dir)
            .map_err(|e| format!("static directory {dir:?} is unusable: {e}"))?;
        if !root.is_dir() {
            return Err(format!("static path {dir:?} is not a directory"));
        }
        Ok(Self { root, index, cache_secs })
    }

    /// Serve `relative` with the whole file in the body. The HTTP server uses
    /// [`Self::open`] and streams instead; this is for an in-process caller
    /// (a tool call) that needs the bytes.
    pub fn serve(&self, relative: &str, request_path: &str, if_none_match: Option<&str>) -> WebCortexResponse {
        match self.open(relative, request_path, if_none_match) {
            FileReply::Done(res) => res,
            FileReply::Stream { mut head, file, len } => {
                use std::io::Read;
                let mut bytes = Vec::with_capacity(len.min(64 * 1024 * 1024) as usize);
                if file.take(len).read_to_end(&mut bytes).is_err() {
                    return WebCortexResponse::coded(404, "WC207", "not found");
                }
                head.body = bytes::Bytes::from(bytes);
                head
            }
        }
    }

    /// Resolve `relative` (the path beneath the mount point) and open it,
    /// without reading it: the server streams the file from disk, so a large
    /// file is never held in memory per request — and a client that reads it
    /// slowly holds a file handle, not the file. `request_path` is the full
    /// path the client asked for, used only to redirect a directory
    /// requested without its trailing slash.
    pub fn open(&self, relative: &str, request_path: &str, if_none_match: Option<&str>) -> FileReply {
        use FileReply::Done;
        let Some(safe) = self.resolve(relative) else {
            // Deliberately 404, not 403: a traversal attempt should learn
            // nothing about what does or does not exist outside the root.
            return Done(WebCortexResponse::coded(404, "WC207", "not found"));
        };

        let target = if safe.is_dir() {
            match &self.index {
                // `/docs/guide` served `guide/index.html` as if it were the
                // file `/docs/guide`, so every relative link in it resolved
                // one directory too high. Redirect to the slash, as every
                // static server does. The Location is relative ("./guide/"),
                // so no request path can turn it into an off-site redirect.
                Some(_) if !request_path.ends_with('/') => {
                    let last = request_path.rsplit('/').next().unwrap_or_default();
                    return Done(WebCortexResponse {
                        status: 301,
                        headers: vec![("location".into(), format!("./{last}/"))],
                        body: bytes::Bytes::new(),
                    });
                }
                Some(name) => safe.join(name),
                None => return Done(WebCortexResponse::coded(404, "WC207", "not found")),
            }
        } else {
            safe
        };

        let not_found = || Done(WebCortexResponse::coded(404, "WC207", "not found"));
        let Ok(file) = std::fs::File::open(&target) else { return not_found() };
        let Ok(meta) = file.metadata() else { return not_found() };
        if !meta.is_file() {
            return not_found();
        }

        let etag = weak_etag(&meta);
        if if_none_match.is_some_and(|v| etag_matches(v, &etag)) {
            return Done(WebCortexResponse {
                status: 304,
                headers: vec![
                    ("etag".into(), etag),
                    ("cache-control".into(), format!("public, max-age={}", self.cache_secs)),
                ],
                body: bytes::Bytes::new(),
            });
        }

        let mime = mime_guess::from_path(&target)
            .first_or_octet_stream()
            .to_string();

        let head = WebCortexResponse {
            status: 200,
            headers: vec![
                ("content-type".into(), mime),
                ("etag".into(), etag),
                (
                    "cache-control".into(),
                    format!("public, max-age={}", self.cache_secs),
                ),
                // Static directories often hold user uploads; never let a
                // browser sniff one into executable content.
                ("x-content-type-options".into(), "nosniff".into()),
            ],
            body: bytes::Bytes::new(),
        };
        FileReply::Stream { head, file, len: meta.len() }
    }

    /// Resolve a request path inside the root, or `None` if it escapes.
    ///
    /// Two independent defences, because either alone has known bypasses:
    /// reject traversal components lexically, then canonicalize and confirm the
    /// result is still under the root (which also catches symlinks pointing
    /// out of the tree).
    fn resolve(&self, relative: &str) -> Option<PathBuf> {
        let decoded = crate::http::percent_decode(relative, false);
        // A NUL byte can truncate a path inside some syscalls.
        if decoded.contains('\0') {
            return None;
        }

        let mut candidate = self.root.clone();
        for component in Path::new(decoded.trim_start_matches('/')).components() {
            match component {
                Component::Normal(part) => {
                    let s = part.to_string_lossy();
                    // Hidden files are not served; `.git` and `.env` under a
                    // static root are otherwise a disclosure waiting to happen.
                    if s.starts_with('.') {
                        return None;
                    }
                    candidate.push(part);
                }
                // Anything else — `..`, a root prefix, a Windows drive letter —
                // is refused outright rather than normalised.
                _ => return None,
            }
        }

        let canonical = std::fs::canonicalize(&candidate).ok()?;
        canonical.starts_with(&self.root).then_some(canonical)
    }
}

/// `If-None-Match` against our tag, by the weak comparison RFC 9110 §13.1.2
/// prescribes: `*`, or any tag in the comma-separated list whose opaque part
/// equals ours, `W/` or not. 2.5.3 compared the whole header to one tag, so a
/// browser or CDN sending a list (or the strong form) never got a 304.
fn etag_matches(header: &str, etag: &str) -> bool {
    let opaque = |t: &str| t.trim().trim_start_matches("W/").to_string();
    let ours = opaque(etag);
    header.split(',').any(|t| t.trim() == "*" || opaque(t) == ours)
}

/// A weak validator from what the filesystem says about the file — its size,
/// modification time and (on Unix) inode — so a file is never read just to
/// tag it. Through 2.8.0 the tag hashed the whole file, read into memory on
/// every request.
fn weak_etag(meta: &std::fs::Metadata) -> String {
    use sha2::{Digest, Sha256};
    let modified = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let inode = 0u64;
    let mut h = Sha256::new();
    h.update(format!("{}:{modified}:{inode}", meta.len()).as_bytes());
    let digest = h.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("W/\"{hex}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("webcortex-files-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
            std::fs::write(dir.join("index.html"), b"<h1>home</h1>").expect("write");
            std::fs::write(dir.join("app.js"), b"console.log(1)").expect("write");
            std::fs::write(dir.join(".env"), b"SECRET=1").expect("write");
            std::fs::write(dir.join("sub/deep.txt"), b"deep").expect("write");
            Self { dir }
        }
        fn server(&self) -> FileServer {
            FileServer::new(
                &self.dir.to_string_lossy(),
                Some("index.html".into()),
                3600,
            )
            .expect("file server")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn serves_a_file_with_a_guessed_mime_type() {
        let f = Fixture::new();
        let res = f.server().serve("/app.js", "/s/app.js", None);
        assert_eq!(res.status, 200);
        let ct = res.headers.iter().find(|(k, _)| k == "content-type").unwrap();
        assert!(ct.1.contains("javascript"), "got {}", ct.1);
    }

    #[test]
    fn serves_the_index_for_a_directory() {
        let f = Fixture::new();
        let res = f.server().serve("/", "/s/", None);
        assert_eq!(res.status, 200);
        assert!(String::from_utf8_lossy(&res.body).contains("home"));
    }

    #[test]
    fn nested_paths_work() {
        let f = Fixture::new();
        assert_eq!(f.server().serve("/sub/deep.txt", "/s/sub/deep.txt", None).status, 200);
    }

    #[test]
    fn traversal_is_refused_in_every_encoding() {
        let f = Fixture::new();
        let s = f.server();
        for attack in [
            "/../../etc/passwd",
            "/..%2f..%2fetc/passwd",
            "/%2e%2e/%2e%2e/etc/passwd",
            "/sub/../../etc/passwd",
            "/....//etc/passwd",
        ] {
            assert_eq!(s.serve(attack, attack, None).status, 404, "leaked via {attack}");
        }
    }

    #[test]
    fn dotfiles_are_never_served() {
        let f = Fixture::new();
        assert_eq!(f.server().serve("/.env", "/s/.env", None).status, 404);
    }

    #[test]
    fn etag_enables_a_304() {
        let f = Fixture::new();
        let s = f.server();
        let first = s.serve("/app.js", "/s/app.js", None);
        let etag = first.headers.iter().find(|(k, _)| k == "etag").unwrap().1.clone();
        let second = s.serve("/app.js", "/s/app.js", Some(&etag));
        assert_eq!(second.status, 304);
        assert!(second.body.is_empty());
    }

    #[test]
    fn missing_file_is_404() {
        let f = Fixture::new();
        assert_eq!(f.server().serve("/nope.txt", "/s/nope.txt", None).status, 404);
    }

    #[test]
    fn if_none_match_uses_the_weak_comparison_over_a_list() {
        let f = Fixture::new();
        let s = f.server();
        let first = s.serve("/app.js", "/s/app.js", None);
        let etag = first.headers.iter().find(|(k, _)| k == "etag").unwrap().1.clone();
        let strong = etag.trim_start_matches("W/").to_string();
        for header in [format!("W/\"other\", {etag}"), strong, "*".to_string(), format!(" {etag} ")] {
            assert_eq!(s.serve("/app.js", "/s/app.js", Some(&header)).status, 304, "{header}");
        }
        assert_eq!(s.serve("/app.js", "/s/app.js", Some("W/\"other\"")).status, 200);
    }

    #[test]
    fn a_file_is_opened_for_streaming_not_read() {
        let f = Fixture::new();
        let big = vec![7u8; 3 * 1024 * 1024];
        std::fs::write(f.dir.join("big.bin"), &big).expect("write");
        match f.server().open("/big.bin", "/s/big.bin", None) {
            FileReply::Stream { head, len, .. } => {
                assert_eq!(head.status, 200);
                assert!(head.body.is_empty(), "the body is streamed, not buffered");
                assert_eq!(len, big.len() as u64);
            }
            FileReply::Done(r) => panic!("expected a stream, got {}", r.status),
        }
        // The in-process path still gets the bytes, and the tag is the same.
        let whole = f.server().serve("/big.bin", "/s/big.bin", None);
        assert_eq!(whole.body.len(), big.len());
        let etag = whole.headers.iter().find(|(k, _)| k == "etag").unwrap().1.clone();
        assert!(matches!(f.server().open("/big.bin", "/s/big.bin", Some(&etag)), FileReply::Done(r) if r.status == 304));
        // A changed file gets a new tag.
        std::fs::write(f.dir.join("big.bin"), b"short").expect("write");
        assert!(matches!(f.server().open("/big.bin", "/s/big.bin", Some(&etag)), FileReply::Stream { .. }));
    }

    #[test]
    fn a_directory_without_its_slash_redirects_relatively() {
        let f = Fixture::new();
        std::fs::write(f.dir.join("sub/index.html"), b"sub").expect("write");
        let res = f.server().serve("/sub", "/s/sub", None);
        assert_eq!(res.status, 301);
        assert_eq!(res.headers, vec![("location".to_string(), "./sub/".to_string())]);
        assert_eq!(f.server().serve("/sub/", "/s/sub/", None).status, 200);
        // The mount point itself, reached without a slash.
        assert_eq!(f.server().serve("/", "/s", None).headers[0].1, "./s/");
    }
}
