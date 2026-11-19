// SPDX-License-Identifier: MIT

//! WebDAV backend (Nextcloud, ownCloud, Synology, Apache `mod_dav`, `rclone serve webdav`).
//!
//! The manifest compare-and-swap relies on HTTP preconditions: `PUT` with
//! `If-None-Match: *` creates only if absent and `PUT` with `If-Match: "<etag>"` replaces
//! only the expected version (`412` means [`PutOutcome::Conflict`]). Only strong ETags are
//! accepted; call [`WebDavBackend::probe_conditional_writes`] at setup to check that the
//! server honours the preconditions at all.
//!
//! Layout below `base_url`: `header`, `manifest`, `seg/<id>`, `probe/<id>`. The `seg/` and
//! `probe/` collections are created with `MKCOL` before the first write (`405` means they
//! exist already). Requests use HTTP Basic authentication (for Nextcloud use an app
//! password), time out after 30 s, never follow redirects and are never retried here.

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use zeroize::Zeroizing;

use super::http::{self, MAX_LIST_KEYS, MAX_META_BYTES, MAX_OBJECT_BYTES};
use super::{ContainerBackend, Etag, PutOutcome, validate_name, validate_prefix};
use crate::{Error, Result};

const IGNORED: &str = "server ignores conditional writes";
const PROPFIND_BODY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/></d:prop></d:propfind>";
/// Deepest element nesting accepted in a multistatus response.
const MAX_XML_DEPTH: usize = 16;

/// Connection settings for [`WebDavBackend`].
pub struct WebDavConfig {
    /// URL of the (existing) collection that holds the container, e.g.
    /// `https://cloud.example.com/remote.php/dav/files/alice/vault`.
    pub base_url: String,
    pub username: String,
    /// Password or Nextcloud app password.
    pub password: Zeroizing<String>,
    /// Allow plain `http://` for non-loopback hosts (default `false`; loopback hosts are
    /// always allowed). Basic credentials are not protected over http.
    pub allow_insecure_http: bool,
    /// Extra PEM root CA(s) trusted in addition to the webpki roots, for self-hosted
    /// servers behind a private CA.
    pub extra_root_ca_pem: Option<String>,
}

impl fmt::Debug for WebDavConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavConfig")
            .field("base_url", &self.base_url)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("extra_root_ca", &self.extra_root_ca_pem.is_some())
            .finish()
    }
}

/// [`ContainerBackend`] over a WebDAV collection.
pub struct WebDavBackend {
    client: reqwest::Client,
    /// Always ends in `/`.
    base: Url,
    /// `Authorization` header, built once and marked sensitive.
    auth: HeaderValue,
    seg_ready: AtomicBool,
    probe_ready: AtomicBool,
}

impl fmt::Debug for WebDavBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavBackend")
            .field("base_url", &self.base.as_str())
            .field("credentials", &"<redacted>")
            .finish()
    }
}

fn bad(msg: &str) -> Error {
    Error::Format(format!("webdav config: {msg}"))
}

enum Cond<'a> {
    IfNoneMatch,
    IfMatch(&'a Etag),
}

fn valid_etag(e: &str) -> bool {
    !e.is_empty() && e.bytes().all(|b| b.is_ascii_graphic() && b != b'"')
}

/// Extracts a strong ETag; weak (`W/`) or malformed values are an error.
fn etag_of(resp: &reqwest::Response) -> Result<Option<Etag>> {
    let Some(v) = resp.headers().get("etag") else {
        return Ok(None);
    };
    let v = v
        .to_str()
        .map_err(|_| Error::Io("webdav: malformed ETag".into()))?
        .trim();
    if v.starts_with("W/") || v.starts_with("w/") {
        return Err(Error::Unsupported(
            "server returns weak ETags (strong ETags required)".into(),
        ));
    }
    let v = v.trim_matches('"');
    if !valid_etag(v) {
        return Err(Error::Io("webdav: malformed ETag".into()));
    }
    Ok(Some(Etag(v.to_string())))
}

fn http_error(op: &str, status: StatusCode) -> Error {
    Error::Io(format!("webdav {op}: http {}", status.as_u16()))
}

impl WebDavBackend {
    /// Validates the configuration and builds the HTTP client (no network I/O).
    pub fn new(cfg: WebDavConfig) -> Result<Self> {
        let mut base = Url::parse(&cfg.base_url).map_err(|_| bad("invalid base URL"))?;
        if base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(bad("base URL must be http(s)://host[:port]/path only"));
        }
        http::check_scheme(&base, cfg.allow_insecure_http).map_err(bad)?;
        if !base.path().ends_with('/') {
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }
        if cfg.username.is_empty()
            || cfg.password.is_empty()
            || cfg.username.contains(':')
            || cfg.username.bytes().any(|b| b.is_ascii_control())
        {
            return Err(bad("invalid or missing credentials"));
        }
        let raw = Zeroizing::new(format!("{}:{}", cfg.username, *cfg.password));
        let encoded = Zeroizing::new(data_encoding::BASE64.encode(raw.as_bytes()));
        let value = Zeroizing::new(format!("Basic {}", *encoded));
        let mut auth = HeaderValue::from_str(&value).map_err(|_| bad("invalid credentials"))?;
        auth.set_sensitive(true);
        let client = http::build_client(cfg.extra_root_ca_pem.as_deref(), "webdav")?;
        Ok(Self {
            client,
            base,
            auth,
            seg_ready: AtomicBool::new(false),
            probe_ready: AtomicBool::new(false),
        })
    }

    /// `rel` consists of validated names only (`[a-z0-9/]`), so it needs no escaping.
    fn url(&self, rel: &str) -> Result<Url> {
        self.base
            .join(rel)
            .map_err(|_| bad("cannot build request URL"))
    }

    async fn send(
        &self,
        method: Method,
        rel: &str,
        headers: &[(&'static str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response> {
        let mut req = self
            .client
            .request(method, self.url(rel)?)
            .header(AUTHORIZATION, self.auth.clone());
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        req.send()
            .await
            .map_err(|e| Error::Io(format!("webdav request failed: {}", e.without_url())))
    }

    /// Creates the collection `dir` (`seg` or `probe`); `405` means it exists already.
    async fn mkcol(&self, dir: &str) -> Result<()> {
        let resp = self
            .send(
                Method::from_bytes(b"MKCOL").unwrap(),
                &format!("{dir}/"),
                &[],
                None,
            )
            .await?;
        let s = resp.status();
        if s.is_success() || s == StatusCode::METHOD_NOT_ALLOWED {
            Ok(())
        } else {
            Err(http_error("mkcol", s))
        }
    }

    fn ready_flag(&self, dir: &str) -> Option<&AtomicBool> {
        match dir {
            "seg" => Some(&self.seg_ready),
            "probe" => Some(&self.probe_ready),
            _ => None,
        }
    }

    async fn put(&self, name: &str, data: &[u8], cond: Cond<'_>) -> Result<PutOutcome> {
        validate_name(name)?;
        let header = match cond {
            Cond::IfNoneMatch => ("if-none-match", "*".to_string()),
            Cond::IfMatch(e) => {
                if !valid_etag(&e.0) {
                    return Err(Error::Format("invalid etag".into()));
                }
                ("if-match", format!("\"{}\"", e.0))
            }
        };
        let dir = name.split_once('/').map(|(d, _)| d);
        let flag = dir.and_then(|d| self.ready_flag(d));
        if let (Some(d), Some(f)) = (dir, flag)
            && !f.load(Ordering::Acquire)
        {
            self.mkcol(d).await?;
            f.store(true, Ordering::Release);
        }
        let headers = [header];
        let mut resp = self
            .send(Method::PUT, name, &headers, Some(data.to_vec()))
            .await?;
        // 409: the parent collection is missing (removed since we created it).
        if let Some(d) = dir
            && resp.status() == StatusCode::CONFLICT
        {
            self.mkcol(d).await?;
            resp = self
                .send(Method::PUT, name, &headers, Some(data.to_vec()))
                .await?;
        }
        let status = resp.status();
        if status.is_success() {
            let etag = match etag_of(&resp)? {
                Some(e) => e,
                None => self.etag_by_get(name, data).await?,
            };
            return Ok(PutOutcome::Written(etag));
        }
        match status {
            StatusCode::PRECONDITION_FAILED => Ok(PutOutcome::Conflict),
            // Some servers answer If-Match on a missing resource with 404.
            StatusCode::NOT_FOUND if matches!(cond, Cond::IfMatch(_)) => Ok(PutOutcome::Conflict),
            StatusCode::NOT_IMPLEMENTED => Err(Error::Unsupported(
                "server does not implement conditional writes".into(),
            )),
            _ => Err(http_error("put", status)),
        }
    }

    /// The server sent no ETag for our `PUT`: fetch it. The body must equal what we wrote,
    /// otherwise the etag could belong to a concurrent writer's version and adopting it
    /// would let a later compare-and-swap overwrite changes we never saw.
    async fn etag_by_get(&self, name: &str, written: &[u8]) -> Result<Etag> {
        let resp = self.send(Method::GET, name, &[], None).await?;
        if !resp.status().is_success() {
            return Err(http_error("put etag lookup", resp.status()));
        }
        let etag = etag_of(&resp)?.ok_or_else(|| Error::Io("webdav put: missing ETag".into()))?;
        let body = http::read_capped(resp, MAX_OBJECT_BYTES, "webdav").await?;
        if body != written {
            return Err(Error::Io(
                "webdav put: object changed before its ETag could be read".into(),
            ));
        }
        Ok(etag)
    }

    /// Checks that the server honours conditional writes and returns strong ETags; call
    /// once at setup.
    ///
    /// Writes a random `probe/` object with `If-None-Match: *` twice (the second must be
    /// refused), tries an `If-Match` write with a wrong etag (must be refused) and removes
    /// the object again. Returns [`Error::Unsupported`] if the server ignores the
    /// preconditions or returns weak or no ETags.
    pub async fn probe_conditional_writes(&self) -> Result<()> {
        let id = data_encoding::HEXLOWER.encode(&rand::random::<[u8; 16]>());
        let name = format!("probe/{id}");
        let result = self.probe_inner(&name).await;
        let cleanup = self.delete(&name).await;
        result?;
        cleanup
    }

    async fn probe_inner(&self, name: &str) -> Result<()> {
        match self.put_if_absent(name, b"probe").await? {
            PutOutcome::Written(_) => {}
            PutOutcome::Conflict => {
                return Err(Error::Io("webdav probe object unexpectedly exists".into()));
            }
        }
        if !matches!(
            self.put_if_absent(name, b"probe2").await?,
            PutOutcome::Conflict
        ) {
            return Err(Error::Unsupported(IGNORED.into()));
        }
        let wrong = Etag("0000000000000000000000000000dead".into());
        if !matches!(
            self.put_if_match(name, b"probe3", &wrong).await?,
            PutOutcome::Conflict
        ) {
            return Err(Error::Unsupported(IGNORED.into()));
        }
        Ok(())
    }
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// From an `href` (absolute path or full URL) returns `(parent segment, last segment)`,
/// both percent-decoded.
fn href_tail(href: &str) -> Option<(String, String)> {
    let href = href.trim();
    let path = if href.starts_with('/') {
        href
    } else if let Some((_, rest)) = href.split_once("://") {
        &rest[rest.find('/')?..]
    } else {
        href
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut segs = path.split('/').filter(|s| !s.is_empty()).rev();
    let last = pct_decode(segs.next()?)?;
    let parent = pct_decode(segs.next()?)?;
    Some((parent, last))
}

/// Parses a `207 Multi-Status` body into the names of the entries directly below `seg/`.
fn parse_multistatus(xml: &str) -> Result<Vec<String>> {
    let fmt_err = |e: &dyn fmt::Display| Error::Format(format!("webdav propfind response: {e}"));
    let mut reader = NsReader::from_str(xml);
    // (is DAV: namespace, local name)
    let mut stack: Vec<(bool, Vec<u8>)> = Vec::new();
    let mut seen_root = false;
    let mut href = String::new();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let (ns, ev) = reader.read_resolved_event().map_err(|e| fmt_err(&e))?;
        match ev {
            Event::DocType(_) => return Err(fmt_err(&"DOCTYPE is not allowed")),
            Event::Start(e) => {
                let dav = matches!(ns, ResolveResult::Bound(n) if n.as_ref() == b"DAV:");
                let name = e.local_name().as_ref().to_vec();
                if stack.is_empty() {
                    if !(dav && name == b"multistatus") {
                        return Err(fmt_err(&"root element is not DAV:multistatus"));
                    }
                    seen_root = true;
                }
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(fmt_err(&"nesting too deep"));
                }
                stack.push((dav, name));
                href.clear();
            }
            Event::Text(t) => {
                if in_href(&stack) {
                    href.push_str(&t.decode().map_err(|e| fmt_err(&e))?);
                }
            }
            Event::CData(c) => {
                if in_href(&stack) {
                    href.push_str(&c.decode().map_err(|e| fmt_err(&e))?);
                }
            }
            Event::GeneralRef(r) => {
                if in_href(&stack) {
                    if let Some(c) = r.resolve_char_ref().map_err(|e| fmt_err(&e))? {
                        href.push(c);
                    } else {
                        let name = r.decode().map_err(|e| fmt_err(&e))?;
                        match quick_xml::escape::resolve_predefined_entity(&name) {
                            Some(s) => href.push_str(s),
                            None => return Err(fmt_err(&"unknown entity")),
                        }
                    }
                }
            }
            Event::End(_) => {
                if in_href(&stack)
                    && let Some((parent, last)) = href_tail(&href)
                    && parent == "seg"
                {
                    let name = format!("seg/{last}");
                    if validate_name(&name).is_ok() && seen.insert(name.clone()) {
                        out.push(name);
                        if out.len() > MAX_LIST_KEYS {
                            return Err(Error::Format("webdav list: too many entries".into()));
                        }
                    }
                }
                stack.pop();
                href.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !seen_root {
        return Err(fmt_err(&"no DAV:multistatus element"));
    }
    Ok(out)
}

/// True inside `DAV:multistatus/DAV:response/DAV:href`.
fn in_href(stack: &[(bool, Vec<u8>)]) -> bool {
    matches!(stack, [(true, a), (true, b), (true, c)]
        if a == b"multistatus" && b == b"response" && c == b"href")
}

#[async_trait::async_trait]
impl ContainerBackend for WebDavBackend {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        validate_name(name)?;
        let resp = self.send(Method::GET, name, &[], None).await?;
        let status = resp.status();
        if status.is_success() {
            let etag =
                etag_of(&resp)?.ok_or_else(|| Error::Io("webdav get: missing ETag".into()))?;
            let body = http::read_capped(resp, MAX_OBJECT_BYTES, "webdav").await?;
            return Ok(Some((body, etag)));
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Err(http_error("get", status))
    }

    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        self.put(name, data, Cond::IfNoneMatch).await
    }

    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        self.put(name, data, Cond::IfMatch(etag)).await
    }

    async fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let resp = self.send(Method::DELETE, name, &[], None).await?;
        let status = resp.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(http_error("delete", status))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        validate_prefix(prefix)?;
        let resp = self
            .send(
                Method::from_bytes(b"PROPFIND").unwrap(),
                prefix,
                &[
                    ("depth", "1".to_string()),
                    ("content-type", "application/xml; charset=utf-8".to_string()),
                ],
                Some(PROPFIND_BODY.as_bytes().to_vec()),
            )
            .await?;
        let status = resp.status();
        // A collection that was never created holds no segments.
        if status == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if status != StatusCode::MULTI_STATUS {
            return Err(http_error("list", status));
        }
        let body = http::read_capped(resp, MAX_META_BYTES, "webdav").await?;
        let xml = std::str::from_utf8(&body)
            .map_err(|_| Error::Format("webdav list response is not UTF-8".into()))?;
        parse_multistatus(xml)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> WebDavConfig {
        WebDavConfig {
            base_url: url.into(),
            username: "alice".into(),
            password: Zeroizing::new("hunter2".into()),
            allow_insecure_http: false,
            extra_root_ca_pem: None,
        }
    }

    #[test]
    fn base_url_is_normalised_and_checked() {
        let b = WebDavBackend::new(cfg("https://c.example.com/remote.php/dav/files/a/v")).unwrap();
        assert_eq!(
            b.url("seg/ab").unwrap().as_str(),
            "https://c.example.com/remote.php/dav/files/a/v/seg/ab"
        );
        let b = WebDavBackend::new(cfg("https://c.example.com")).unwrap();
        assert_eq!(
            b.url("manifest").unwrap().as_str(),
            "https://c.example.com/manifest"
        );
        for bad in [
            "ftp://x/",
            "https://u:p@x/",
            "https://x/?a=1",
            "https://x/#f",
            "nonsense",
            "http://c.example.com/dav",
        ] {
            assert!(WebDavBackend::new(cfg(bad)).is_err(), "{bad}");
        }
        for ok in [
            "http://127.0.0.1:8080/dav",
            "http://localhost/dav",
            "http://[::1]/d",
        ] {
            assert!(WebDavBackend::new(cfg(ok)).is_ok(), "{ok}");
        }
        let mut c = cfg("http://nas.lan/dav");
        c.allow_insecure_http = true;
        assert!(WebDavBackend::new(c).is_ok());
    }

    #[test]
    fn credentials_are_checked() {
        for (u, p) in [("", "x"), ("a", ""), ("a:b", "x")] {
            let mut c = cfg("https://x/");
            c.username = u.into();
            c.password = Zeroizing::new(p.into());
            assert!(WebDavBackend::new(c).is_err(), "{u:?}");
        }
    }

    #[test]
    fn auth_header_is_sensitive_basic() {
        let b = WebDavBackend::new(cfg("https://x/")).unwrap();
        assert!(b.auth.is_sensitive());
        assert_eq!(b.auth.to_str().unwrap(), "Basic YWxpY2U6aHVudGVyMg==");
    }

    #[test]
    fn extra_root_ca_must_parse() {
        for bad in ["", "not a pem"] {
            let mut c = cfg("https://x/");
            c.extra_root_ca_pem = Some(bad.into());
            assert!(WebDavBackend::new(c).is_err(), "{bad:?}");
        }
    }

    const A: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn parses_prefixes_encodings_and_absolute_hrefs() {
        let xml = format!(
            r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:o="urn:x">
<D:response><D:href>/dav/v/seg/</D:href></D:response>
<D:response><D:href>/dav/v/seg/%30%30112233445566778899aabbccddeeff</D:href></D:response>
<D:response><D:href>https://h.example:8443/dav/v/seg/{b}?x=1</D:href></D:response>
<D:response><D:href><![CDATA[/dav/v/seg/{c}]]></D:href></D:response>
<D:response><D:href>/dav/v/seg/{A}</D:href></D:response>
<D:response><D:href>/dav/v/seg/notes.txt</D:href></D:response>
<D:response><D:href>/dav/v/seg/%2e%2e</D:href></D:response>
<D:response><D:href>/dav/v/other/{d}</D:href></D:response>
<D:response><D:href>/dav/v/seg/{A}%0a</D:href></D:response>
<D:response><D:href>/dav/v/seg/%ff%fe</D:href></D:response>
<o:response><o:href>/dav/v/seg/{e}</o:href></o:response>
<D:response><D:propstat><o:href>/dav/v/seg/{e}</o:href></D:propstat></D:response>
</D:multistatus>"#,
            b = "ffeeddccbbaa99887766554433221100",
            c = "0123456789abcdef0123456789abcdef",
            d = "11111111111111111111111111111111",
            e = "22222222222222222222222222222222",
        );
        let got = parse_multistatus(&xml).unwrap();
        assert_eq!(
            got,
            vec![
                format!("seg/{A}"),
                "seg/ffeeddccbbaa99887766554433221100".to_string(),
                "seg/0123456789abcdef0123456789abcdef".to_string(),
            ]
        );
    }

    #[test]
    fn rejects_doctype_wrong_root_and_garbage() {
        for xml in [
            "<?xml version=\"1.0\"?><!DOCTYPE x><d:multistatus xmlns:d=\"DAV:\"/>",
            "<multistatus/>",
            "<d:multistatus xmlns:d=\"urn:other\"></d:multistatus>",
            "<html></html>",
            "",
            "<d:multistatus xmlns:d=\"DAV:\"><d:response></d:multistatus>",
            "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>&bogus;</d:href></d:response></d:multistatus>",
        ] {
            assert!(parse_multistatus(xml).is_err(), "{xml}");
        }
        let deep = format!(
            "<d:multistatus xmlns:d=\"DAV:\">{}{}</d:multistatus>",
            "<d:a>".repeat(40),
            "</d:a>".repeat(40)
        );
        assert!(parse_multistatus(&deep).is_err());
    }

    #[test]
    fn empty_multistatus_is_an_empty_list() {
        assert!(parse_multistatus("<d:multistatus xmlns:d=\"DAV:\"/>").is_err());
        assert!(
            parse_multistatus("<d:multistatus xmlns:d=\"DAV:\"></d:multistatus>")
                .unwrap()
                .is_empty()
        );
    }
}
