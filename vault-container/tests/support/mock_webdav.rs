// SPDX-License-Identifier: MIT

//! In-process WebDAV mock with a Nextcloud-like base path. Requires HTTP Basic auth,
//! enforces `If-None-Match` / `If-Match` with strong ETags, creates collections with
//! MKCOL (405 when present, 409 without parent) and answers PROPFIND with percent-encoded
//! hrefs. [`Mode`] switches in the misbehaviours the backend must cope with.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};

pub const USER: &str = "user";
pub const PASSWORD: &str = "app-pass-123";
/// Collection the backend is pointed at (relative to the server root).
pub const BASE_PATH: &str = "/remote.php/dav/files/user/vault";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Enforce,
    /// Ignores `If-None-Match` / `If-Match` (plain `mod_dav` without preconditions).
    IgnoreConditions,
    /// ETags are weak (`W/"..."`).
    WeakEtags,
    /// PUT responses carry no ETag header (GET/HEAD still do).
    NoEtagOnPut,
    /// PUT carries no ETag and HEAD/GET return another writer's content.
    NoEtagOnPutRacy,
    /// PROPFIND body is 16 MiB + 1 bytes.
    ListHuge,
    /// PROPFIND lists 100_001 valid segment names.
    ListTooMany,
    /// PROPFIND answers with a DOCTYPE carrying an entity bomb.
    ListDoctype,
    /// PROPFIND answers with an unrelated 200 body (not multistatus).
    ListNotMultistatus,
}

#[derive(Default)]
struct State {
    /// Full decoded paths of files.
    files: BTreeMap<String, (Vec<u8>, String)>,
    /// Full decoded paths of collections (no trailing slash).
    collections: BTreeSet<String>,
    requests: usize,
    mkcols: usize,
    authorizations: Vec<String>,
    methods: Vec<String>,
}

pub struct MockWebDav {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl MockWebDav {
    pub fn start(mode: Mode) -> MockWebDav {
        let std_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_l.set_nonblocking(true).unwrap();
        let addr = std_l.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(std_l).unwrap();
        let mut st = State::default();
        let mut p = String::new();
        for seg in BASE_PATH.split('/').filter(|s| !s.is_empty()) {
            p.push('/');
            p.push_str(seg);
            st.collections.insert(p.clone());
        }
        let state = Arc::new(Mutex::new(st));
        let st = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    continue;
                };
                let st = Arc::clone(&st);
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let st = Arc::clone(&st);
                        async move { Ok::<_, Infallible>(handle(req, st, mode).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(sock), svc)
                        .await;
                });
            }
        });
        MockWebDav { addr, state }
    }

    /// URL of the vault collection, as a user would configure it.
    pub fn base_url(&self) -> String {
        format!("http://{}{}", self.addr, BASE_PATH)
    }

    pub fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests
    }

    pub fn mkcol_count(&self) -> usize {
        self.state.lock().unwrap().mkcols
    }

    pub fn methods(&self) -> Vec<String> {
        self.state.lock().unwrap().methods.clone()
    }

    /// Every `Authorization` header value received (missing ones are not recorded).
    pub fn authorizations(&self) -> Vec<String> {
        self.state.lock().unwrap().authorizations.clone()
    }

    /// File names relative to the vault collection.
    pub fn files(&self) -> Vec<String> {
        let prefix = format!("{BASE_PATH}/");
        let g = self.state.lock().unwrap();
        g.files
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    /// Plants a file (and its parent collection) bypassing the protocol.
    pub fn plant(&self, rel: &str, data: &[u8]) {
        let mut g = self.state.lock().unwrap();
        let full = format!("{BASE_PATH}/{rel}");
        if let Some((dir, _)) = rel.rsplit_once('/') {
            g.collections.insert(format!("{BASE_PATH}/{dir}"));
        }
        let etag = etag_of(data, false);
        g.files.insert(full, (data.to_vec(), etag));
    }

    /// Removes a collection and everything below it.
    pub fn remove_collection(&self, rel: &str) {
        let mut g = self.state.lock().unwrap();
        let full = format!("{BASE_PATH}/{rel}");
        let under = format!("{full}/");
        g.collections.remove(&full);
        g.files.retain(|k, _| !k.starts_with(&under));
    }
}

fn etag_of(data: &[u8], weak: bool) -> String {
    let h = data_encoding::HEXLOWER.encode(&Sha256::digest(data));
    if weak {
        format!("W/\"{h}\"")
    } else {
        format!("\"{h}\"")
    }
}

fn resp(status: u16, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .unwrap()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

/// Percent-encodes everything but unreserved characters and `/`; the first character
/// of the last segment is always encoded, so clients must decode hrefs.
fn encode_href(path: &str) -> String {
    let (dir, last) = path.rsplit_once('/').unwrap_or(("", path));
    let mut out = String::new();
    for (i, seg) in dir.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&enc(seg, false));
    }
    out.push('/');
    out.push_str(&enc(last, true));
    out
}

fn encode_path(p: &str) -> String {
    p.split('/')
        .map(|s| enc(s, false))
        .collect::<Vec<_>>()
        .join("/")
}

fn enc(s: &str, force_first: bool) -> String {
    let mut out = String::new();
    for (i, b) in s.bytes().enumerate() {
        if (b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
            && !(force_first && i == 0)
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn handle(
    req: Request<Incoming>,
    st: Arc<Mutex<State>>,
    mode: Mode,
) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    {
        let mut g = st.lock().unwrap();
        g.requests += 1;
        g.methods.push(parts.method.to_string());
        if let Some(a) = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        {
            g.authorizations.push(a.to_string());
        }
    }
    let want = format!(
        "Basic {}",
        data_encoding::BASE64.encode(format!("{USER}:{PASSWORD}").as_bytes())
    );
    if parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        != Some(want.as_str())
    {
        return Response::builder()
            .status(401)
            .header("www-authenticate", "Basic realm=\"mock\"")
            .body(Full::new(Bytes::new()))
            .unwrap();
    }
    let path = pct_decode(parts.uri.path());
    let path = path.trim_end_matches('/').to_string();
    let hdr = |n: &str| {
        parts
            .headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let weak = mode == Mode::WeakEtags;
    let parent_exists = |g: &State, p: &str| {
        p.rsplit_once('/')
            .is_some_and(|(dir, _)| g.collections.contains(dir))
    };

    match parts.method.as_str() {
        "MKCOL" => {
            let mut g = st.lock().unwrap();
            g.mkcols += 1;
            if g.collections.contains(&path) || g.files.contains_key(&path) {
                return resp(405, "");
            }
            if !parent_exists(&g, &path) {
                return resp(409, "");
            }
            g.collections.insert(path);
            resp(201, "")
        }
        "GET" | "HEAD" => {
            let g = st.lock().unwrap();
            let Some((data, etag)) = g.files.get(&path) else {
                return resp(404, "");
            };
            let mut data = data.clone();
            if mode == Mode::NoEtagOnPutRacy {
                data = b"someone else wrote this".to_vec();
            }
            let b = Response::builder()
                .status(200)
                .header("etag", etag.as_str())
                .header("content-length", data.len());
            if parts.method == Method::HEAD {
                b.body(Full::new(Bytes::new())).unwrap()
            } else {
                b.body(Full::new(Bytes::from(data))).unwrap()
            }
        }
        "PUT" => {
            let if_none = hdr("if-none-match");
            let if_match = hdr("if-match");
            // The lock is held across check and write: conditional writes are atomic.
            let mut g = st.lock().unwrap();
            if !parent_exists(&g, &path) || g.collections.contains(&path) {
                return resp(409, "");
            }
            if mode != Mode::IgnoreConditions {
                let current = g.files.get(&path).map(|(_, e)| e.clone());
                if if_none.as_deref() == Some("*") && current.is_some() {
                    return resp(412, "");
                }
                if let Some(m) = if_match {
                    match current {
                        // RFC 9110: If-Match on a missing resource fails with 412.
                        None => return resp(412, ""),
                        Some(c) if c != m => return resp(412, ""),
                        _ => {}
                    }
                }
            }
            let etag = etag_of(&body, weak);
            let existed = g
                .files
                .insert(path, (body.to_vec(), etag.clone()))
                .is_some();
            let mut b = Response::builder().status(if existed { 204 } else { 201 });
            if !matches!(mode, Mode::NoEtagOnPut | Mode::NoEtagOnPutRacy) {
                b = b.header("etag", etag);
            }
            b.body(Full::new(Bytes::new())).unwrap()
        }
        "DELETE" => {
            let mut g = st.lock().unwrap();
            if g.files.remove(&path).is_some() {
                resp(204, "")
            } else {
                resp(404, "")
            }
        }
        "PROPFIND" => {
            if hdr("depth").as_deref() != Some("1") {
                return resp(400, "depth must be 1");
            }
            if !body.windows(4).any(|w| w == b"prop") {
                return resp(400, "missing propfind body");
            }
            let g = st.lock().unwrap();
            if !g.collections.contains(&path) {
                return resp(404, "");
            }
            propfind(&g, &path, mode)
        }
        _ => resp(405, ""),
    }
}

fn propfind(g: &State, dir: &str, mode: Mode) -> Response<Full<Bytes>> {
    const HEAD: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\">";
    let entry = |href: &str, coll: bool| {
        format!(
            "<d:response><d:href>{href}</d:href><d:propstat><d:prop><d:resourcetype>{}</d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
            if coll { "<d:collection/>" } else { "" }
        )
    };
    match mode {
        Mode::ListHuge => return resp(207, vec![b' '; (16 << 20) + 1]),
        Mode::ListDoctype => {
            return resp(
                207,
                "<?xml version=\"1.0\"?><!DOCTYPE d:multistatus [<!ENTITY a \"aaaaaaaaaa\"><!ENTITY b \"&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;\">]><d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/&b;</d:href></d:response></d:multistatus>",
            );
        }
        Mode::ListNotMultistatus => return resp(207, "<html><body>captive portal</body></html>"),
        Mode::ListTooMany => {
            let mut xml = String::from(HEAD);
            for i in 0..100_001u64 {
                xml.push_str(&format!(
                    "<d:response><d:href>{dir}/{i:032x}</d:href></d:response>"
                ));
            }
            xml.push_str("</d:multistatus>");
            return resp(207, xml);
        }
        _ => {}
    }
    let mut xml = String::from(HEAD);
    xml.push_str(&entry(&format!("{}/", encode_path(dir)), true));
    let under = format!("{dir}/");
    for k in g
        .files
        .keys()
        .filter(|k| k.starts_with(&under) && !k[under.len()..].contains('/'))
    {
        xml.push_str(&entry(&encode_href(k), false));
    }
    for c in g
        .collections
        .iter()
        .filter(|c| c.starts_with(&under) && !c[under.len()..].contains('/'))
    {
        xml.push_str(&entry(&format!("{}/", encode_path(c)), true));
    }
    xml.push_str("</d:multistatus>");
    resp(207, xml)
}
