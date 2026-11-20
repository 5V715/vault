// SPDX-License-Identifier: MIT
//! Read-only static files next to the relay (`vault-relay --static-dir`), for
//! images that serve a page and its relay on one port (`vault-todo-demo`).
//! Off unless a [`StaticSite`] is passed to [`crate::serve_with`].
//!
//! What it serves, and nothing else:
//! - `GET`/`HEAD` only (anything else: 405); `/` is `index.html`.
//! - Paths whose every segment is `[A-Za-z0-9_-][A-Za-z0-9._-]*`: no percent
//!   escapes at all (so no `%2e%2e`, `%2f` or `%00`), no backslashes, no empty or
//!   dot segments (so no `..`, no dot-files such as `.build.json`).
//! - Regular files only (no directory listings, the file itself not a symlink),
//!   whose canonical path is still under the canonical root (a symlinked
//!   directory pointing outside is refused), with a known extension.
//! - `/config.json` from memory: `{"relay":"<relay ws origin>"}`, how the page
//!   learns its relay at run time.
//!
//! Every response carries the page's exact CSP (computed once, here), `nosniff`,
//! `Referrer-Policy: no-referrer`, `Cross-Origin-Opener-Policy: same-origin` and
//! `Cache-Control: no-store`. The root is assumed not to change while serving
//! (a read-only image layer); a symlink swapped in between the check and the read
//! is not defended against.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{
    ALLOW, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HeaderValue, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS,
};
use hyper::{Method, Response, StatusCode};

type Resp = Response<Full<Bytes>>;

/// Largest file served (the page's bundle with the wasm inside is well under this).
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// A directory served read-only with the page's headers, plus `/config.json`.
#[derive(Debug)]
pub struct StaticSite {
    root: PathBuf,
    csp: HeaderValue,
    config_json: Bytes,
}

impl StaticSite {
    /// `root` must be an existing directory. `page_origin` is where the page is
    /// served (`https://host[:port]`, `http` for local testing) and `relay_origin`
    /// the relay's WebSocket origin (`wss://host[:port]` or `ws://`), both already
    /// in canonical form (lower case, no default port, no path): `vault-relay`
    /// normalizes them with the app-binding origin rules. This only rejects
    /// anything that could break out of the CSP header or the JSON.
    pub fn new(root: &Path, page_origin: &str, relay_origin: &str) -> Result<Self, String> {
        check_origin(page_origin, &["https", "http"], "page origin")?;
        check_origin(relay_origin, &["wss", "ws"], "relay origin")?;
        let root = std::fs::canonicalize(root)
            .map_err(|e| format!("static directory {}: {e}", root.display()))?;
        if !root.is_dir() {
            return Err(format!(
                "static directory {} is not a directory",
                root.display()
            ));
        }
        let csp = csp(relay_origin);
        Ok(Self {
            root,
            csp: HeaderValue::from_str(&csp).map_err(|_| "CSP is not a header value")?,
            config_json: Bytes::from(format!("{{\"relay\":\"{relay_origin}\"}}\n")),
        })
    }

    /// The `Content-Security-Policy` every response carries.
    pub fn csp(&self) -> &str {
        self.csp.to_str().unwrap_or_default()
    }

    /// The canonical root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Answers one request for `path` (the request URI's path, still percent-encoded).
    pub(crate) async fn respond(&self, method: &Method, path: &str) -> Resp {
        // HEAD gets the GET response; hyper drops the body but keeps its length.
        if method != Method::GET && method != Method::HEAD {
            let mut r = self.reply(StatusCode::METHOD_NOT_ALLOWED, TEXT, "method not allowed\n");
            r.headers_mut()
                .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
            return r;
        }
        let (body, ty) = if path == "/config.json" {
            (self.config_json.clone(), "application/json")
        } else {
            match self.read(path).await {
                Some(v) => v,
                None => return self.reply(StatusCode::NOT_FOUND, TEXT, "not found\n"),
            }
        };
        self.reply(StatusCode::OK, ty, body)
    }

    async fn read(&self, path: &str) -> Option<(Bytes, &'static str)> {
        let rel = safe_relative(path)?;
        let ty = content_type(&rel)?;
        let full = self.root.join(&rel);
        // The file itself must not be a symlink, and must be a regular file.
        let meta = tokio::fs::symlink_metadata(&full).await.ok()?;
        if !meta.file_type().is_file() || meta.len() > MAX_FILE_BYTES {
            return None;
        }
        // A symlinked directory on the way must not lead outside the root.
        let real = tokio::fs::canonicalize(&full).await.ok()?;
        if !real.starts_with(&self.root) || real == self.root {
            return None;
        }
        let body = tokio::fs::read(&real).await.ok()?;
        Some((Bytes::from(body), ty))
    }

    fn reply(&self, status: StatusCode, ty: &'static str, body: impl Into<Bytes>) -> Resp {
        let mut r = Response::new(Full::new(body.into()));
        *r.status_mut() = status;
        let h = r.headers_mut();
        h.insert(CONTENT_TYPE, HeaderValue::from_static(ty));
        h.insert(CONTENT_SECURITY_POLICY, self.csp.clone());
        h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        h.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        h.insert(
            "cross-origin-opener-policy",
            HeaderValue::from_static("same-origin"),
        );
        h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    }
}

const TEXT: &str = "text/plain; charset=utf-8";

/// The page's policy (the to-do plan's, with `connect-src 'self'` added so the
/// page can fetch `/config.json`, and the relay's WebSocket origin).
fn csp(relay_ws_origin: &str) -> String {
    [
        "default-src 'none'",
        "script-src 'self' 'wasm-unsafe-eval'",
        "style-src 'self'",
        "img-src 'self' data:",
        &format!("connect-src 'self' {relay_ws_origin}"),
        "base-uri 'none'",
        "form-action 'none'",
        "frame-ancestors 'none'",
    ]
    .join("; ")
}

/// `scheme://authority` with an allowed scheme and an authority of lower-case
/// host characters, brackets (IPv6) and a port only.
fn check_origin(v: &str, schemes: &[&str], what: &str) -> Result<(), String> {
    let bad = || {
        Err(format!(
            "bad {what} {v:?}: expected canonical {}://host[:port]",
            schemes[0]
        ))
    };
    let Some((scheme, authority)) = v.split_once("://") else {
        return bad();
    };
    let ok = schemes.contains(&scheme)
        && !authority.is_empty()
        && v.len() <= 300
        && authority.bytes().all(|c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, b'.' | b'-' | b':' | b'[' | b']')
        });
    if ok { Ok(()) } else { bad() }
}

/// The relative file path that the URI `path` names, if every segment is a plain
/// name (see the module docs); `/` is `index.html`.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix('/')?;
    if rest.is_empty() {
        return Some(PathBuf::from("index.html"));
    }
    let mut out = PathBuf::new();
    for seg in rest.split('/') {
        let b = seg.as_bytes();
        let plain = |c: &u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-');
        if b.is_empty() || b[0] == b'.' || !b.iter().all(plain) {
            return None;
        }
        out.push(seg);
    }
    Some(out)
}

fn content_type(rel: &Path) -> Option<&'static str> {
    let ext = rel.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_relative_accepts_plain_names_only() {
        assert_eq!(safe_relative("/"), Some(PathBuf::from("index.html")));
        assert_eq!(
            safe_relative("/a/b-c_d.e.js"),
            Some(PathBuf::from("a/b-c_d.e.js"))
        );
        for bad in [
            "", "a", "//", "/a/", "/..", "/a/../b", "/.x", "/%2e%2e", "/a%2fb", "/a\\b", "/a\0",
            "/a b", "/é",
        ] {
            assert_eq!(safe_relative(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn csp_is_the_plan_policy_with_self() {
        assert_eq!(
            csp("wss://todo.example"),
            "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; \
             img-src 'self' data:; connect-src 'self' wss://todo.example; base-uri 'none'; \
             form-action 'none'; frame-ancestors 'none'"
        );
    }
}
