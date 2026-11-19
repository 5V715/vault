// SPDX-License-Identifier: MIT

//! In-process S3 mock (path-style) with conditional-write semantics and ListObjectsV2
//! pagination. Requires a well-formed SigV4 `Authorization` header and a matching
//! `x-amz-content-sha256`, but does not verify the signature itself (that is covered by
//! the AWS test vectors).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Enforces `If-None-Match` / `If-Match` like AWS S3.
    Enforce,
    /// Silently ignores conditional headers (old S3-compatibles).
    IgnoreConditionals,
    /// Answers conditional writes with `409 ConditionalRequestConflict`.
    Conflict409,
    /// Answers everything with this status.
    Fail(u16),
    /// GET object returns 64 MiB + 1 bytes, list returns 16 MiB + 1 bytes.
    Oversize,
    /// List always answers an empty truncated page with the same token.
    StuckList,
    /// List alternates between tokens "A" and "B" with empty truncated pages.
    AlternatingList,
    /// List answers empty truncated pages with ever-new tokens.
    EndlessList,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, (Vec<u8>, String)>,
    requests: usize,
}

pub struct MockS3 {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl MockS3 {
    /// Starts a server on an ephemeral loopback port for bucket `bucket`; must be called
    /// inside a tokio runtime. The server lives until the runtime shuts down.
    pub fn start(bucket: &str, mode: Mode, page_size: usize) -> MockS3 {
        let std_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_l.set_nonblocking(true).unwrap();
        let addr = std_l.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(std_l).unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let st = Arc::clone(&state);
        let bucket = bucket.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    continue;
                };
                let (st, bucket) = (Arc::clone(&st), bucket.clone());
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let (st, bucket) = (Arc::clone(&st), bucket.clone());
                        async move {
                            Ok::<_, Infallible>(handle(req, st, &bucket, mode, page_size).await)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(sock), svc)
                        .await;
                });
            }
        });
        MockS3 { addr, state }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Number of requests served so far.
    pub fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests
    }

    /// Full object keys (without bucket) currently stored.
    pub fn keys(&self) -> Vec<String> {
        self.state.lock().unwrap().objects.keys().cloned().collect()
    }
}

fn resp(status: u16, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .unwrap()
}

fn s3_error(status: u16, code: &str) -> Response<Full<Bytes>> {
    resp(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>mock</Message></Error>"
        ),
    )
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Checks the shape of a SigV4 `Authorization` header.
fn auth_well_formed(headers: &hyper::HeaderMap, payload: &[u8]) -> Result<(), &'static str> {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    let auth = h("authorization").ok_or("missing authorization")?;
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or("bad algorithm")?;
    let parts: Vec<&str> = rest.split(", ").collect();
    if parts.len() != 3 {
        return Err("bad parts");
    }
    let cred = parts[0]
        .strip_prefix("Credential=")
        .ok_or("no credential")?;
    let scope: Vec<&str> = cred.split('/').collect();
    if scope.len() != 5 || scope[0].is_empty() || scope[3] != "s3" || scope[4] != "aws4_request" {
        return Err("bad credential scope");
    }
    let date = h("x-amz-date").ok_or("missing x-amz-date")?;
    if date.len() != 16
        || !date.ends_with('Z')
        || date.as_bytes()[8] != b'T'
        || !date[..8].bytes().all(|b| b.is_ascii_digit())
        || scope[1] != &date[..8]
    {
        return Err("bad x-amz-date");
    }
    let signed = parts[1]
        .strip_prefix("SignedHeaders=")
        .ok_or("no signed headers")?;
    let names: Vec<&str> = signed.split(';').collect();
    let mut sorted = names.clone();
    sorted.sort();
    if names != sorted {
        return Err("signed headers unsorted");
    }
    for need in ["host", "x-amz-content-sha256", "x-amz-date"] {
        if !names.contains(&need) {
            return Err("required header not signed");
        }
    }
    for cond in ["if-match", "if-none-match"] {
        if headers.contains_key(cond) && !names.contains(&cond) {
            return Err("conditional header not signed");
        }
    }
    let sig = parts[2].strip_prefix("Signature=").ok_or("no signature")?;
    if sig.len() != 64 || !sig.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("bad signature");
    }
    let want = h("x-amz-content-sha256").ok_or("missing content sha")?;
    let got = data_encoding::HEXLOWER.encode(&Sha256::digest(payload));
    if want != got {
        return Err("payload hash mismatch");
    }
    Ok(())
}

async fn handle(
    req: Request<Incoming>,
    st: Arc<Mutex<State>>,
    bucket: &str,
    mode: Mode,
    page_size: usize,
) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let req = Request::from_parts(parts, ());
    st.lock().unwrap().requests += 1;
    if let Mode::Fail(code) = mode {
        return s3_error(code, "InternalError");
    }
    if let Err(why) = auth_well_formed(req.headers(), &body) {
        return s3_error(403, &format!("AccessDenied{}", why.replace(' ', "")));
    }

    let path = req.uri().path();
    let query = req.uri().query().unwrap_or("");
    let rest = match path.strip_prefix('/').and_then(|p| p.strip_prefix(bucket)) {
        Some(r) => r,
        None => return s3_error(404, "NoSuchBucket"),
    };
    let key = rest
        .strip_prefix('/')
        .map(pct_decode)
        .filter(|k| !k.is_empty());
    let hdr = |n: &str| {
        req.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let etag_of = |data: &[u8]| {
        format!(
            "\"{}\"",
            data_encoding::HEXLOWER.encode(&Sha256::digest(data))
        )
    };

    match (req.method(), key) {
        (&Method::GET, None) if mode == Mode::Oversize => resp(200, vec![b'x'; (16 << 20) + 1]),
        (&Method::GET, None)
            if matches!(
                mode,
                Mode::StuckList | Mode::AlternatingList | Mode::EndlessList
            ) =>
        {
            let tok = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("continuation-token="))
                .map(pct_decode);
            let next = match mode {
                Mode::StuckList => "same".to_string(),
                Mode::AlternatingList => if tok.as_deref() == Some("A") {
                    "B"
                } else {
                    "A"
                }
                .to_string(),
                _ => tok
                    .and_then(|t| t.parse::<u64>().ok())
                    .map_or(1, |n| n + 1)
                    .to_string(),
            };
            resp(
                200,
                format!(
                    "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>{next}</NextContinuationToken></ListBucketResult>"
                ),
            )
        }
        (&Method::GET, None) => list(&st, query, page_size),
        (&Method::GET, Some(_)) if mode == Mode::Oversize => resp(200, vec![0u8; (64 << 20) + 1]),
        (&Method::GET, Some(k)) => {
            let g = st.lock().unwrap();
            match g.objects.get(&k) {
                Some((data, etag)) => Response::builder()
                    .status(StatusCode::OK)
                    .header("etag", etag.as_str())
                    .body(Full::new(Bytes::from(data.clone())))
                    .unwrap(),
                None => s3_error(404, "NoSuchKey"),
            }
        }
        (&Method::PUT, Some(k)) => {
            let if_none = hdr("if-none-match");
            let if_match = hdr("if-match");
            if mode == Mode::Conflict409 && (if_none.is_some() || if_match.is_some()) {
                return s3_error(409, "ConditionalRequestConflict");
            }
            // The lock is held across check and write: conditional writes are atomic.
            let mut g = st.lock().unwrap();
            if mode == Mode::Enforce {
                let current = g.objects.get(&k).map(|(_, e)| e.clone());
                if if_none.as_deref() == Some("*") && current.is_some() {
                    return s3_error(412, "PreconditionFailed");
                }
                if let Some(m) = if_match {
                    match current {
                        None => return s3_error(404, "NoSuchKey"),
                        Some(c) if c != m => return s3_error(412, "PreconditionFailed"),
                        _ => {}
                    }
                }
            }
            let etag = etag_of(&body);
            g.objects.insert(k, (body.to_vec(), etag.clone()));
            Response::builder()
                .status(200)
                .header("etag", etag)
                .body(Full::new(Bytes::new()))
                .unwrap()
        }
        (&Method::DELETE, Some(k)) => {
            st.lock().unwrap().objects.remove(&k);
            resp(204, "")
        }
        _ => s3_error(405, "MethodNotAllowed"),
    }
}

fn list(st: &Arc<Mutex<State>>, query: &str, page_size: usize) -> Response<Full<Bytes>> {
    let mut prefix = String::new();
    let mut token: Option<String> = None;
    let mut list_type = false;
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let v = pct_decode(v);
        match k {
            "prefix" => prefix = v,
            "continuation-token" => token = Some(v),
            "list-type" => list_type = v == "2",
            _ => {}
        }
    }
    if !list_type {
        return s3_error(400, "InvalidArgument");
    }
    let after = match token {
        None => None,
        Some(t) => match data_encoding::HEXLOWER.decode(t.as_bytes()) {
            Ok(b) => Some(String::from_utf8(b).unwrap()),
            Err(_) => return s3_error(400, "InvalidArgument"),
        },
    };
    let g = st.lock().unwrap();
    let matching: Vec<&String> = g
        .objects
        .keys()
        .filter(|k| k.starts_with(&prefix) && after.as_ref().is_none_or(|a| *k > a))
        .collect();
    let page: Vec<&&String> = matching.iter().take(page_size).collect();
    let truncated = matching.len() > page.len();
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    xml.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(&prefix)));
    xml.push_str(&format!("<KeyCount>{}</KeyCount>", page.len()));
    xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
    if truncated {
        let last = page.last().unwrap();
        xml.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            data_encoding::HEXLOWER.encode(last.as_bytes())
        ));
    }
    for k in &page {
        xml.push_str(&format!(
            "<Contents><Key>{}</Key></Contents>",
            xml_escape(k)
        ));
    }
    xml.push_str("</ListBucketResult>");
    resp(200, xml)
}
