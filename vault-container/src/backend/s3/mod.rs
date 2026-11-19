// SPDX-License-Identifier: MIT

//! S3-compatible backend (AWS S3, MinIO, Garage, ...).
//!
//! Requires conditional writes (`If-None-Match: *` and `If-Match`), which the manifest
//! compare-and-swap depends on; call [`S3Backend::probe_conditional_writes`] at setup to
//! check that the provider honours them. Requests are signed with SigV4 (hand-written,
//! payload always hashed), time out after 30 s and are never retried here: the
//! replication layer owns retry policy.

mod sigv4;

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest::{Method, StatusCode, Url};
use zeroize::Zeroizing;

use super::http::{self, MAX_LIST_KEYS, MAX_META_BYTES, MAX_OBJECT_BYTES};
use super::{ContainerBackend, Etag, PutOutcome, validate_name, validate_prefix};
use crate::{Error, Result};

/// Upper bound on ListObjectsV2 pages per list call.
const MAX_LIST_PAGES: usize = 10_000;
/// Maximum element nesting accepted in a list response (as for WebDAV).
const MAX_XML_DEPTH: usize = 16;

/// Connection settings for [`S3Backend`].
pub struct S3Config {
    /// Base URL, e.g. `https://s3.eu-central-1.amazonaws.com` or `http://127.0.0.1:9000`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix (`[A-Za-z0-9._/-]`, no leading/trailing `/`, no `..`); may be empty.
    pub prefix: String,
    pub access_key: String,
    pub secret_key: Zeroizing<String>,
    /// `true`: `endpoint/bucket/key`; `false`: `bucket.endpoint/key`.
    pub path_style: bool,
    /// Allow plain `http://` for non-loopback hosts (default `false`; loopback hosts
    /// are always allowed). Requests are signed but not encrypted over http.
    pub allow_insecure_http: bool,
    /// Extra PEM root CA(s) trusted in addition to the webpki roots, for self-hosted
    /// providers behind a private CA.
    pub extra_root_ca_pem: Option<String>,
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field("path_style", &self.path_style)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("extra_root_ca", &self.extra_root_ca_pem.is_some())
            .finish()
    }
}

/// [`ContainerBackend`] over an S3-compatible bucket.
pub struct S3Backend {
    client: reqwest::Client,
    base: Url,
    region: String,
    bucket: String,
    prefix: String,
    access_key: String,
    secret_key: Zeroizing<String>,
    path_style: bool,
}

impl fmt::Debug for S3Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Backend")
            .field("endpoint", &self.base.as_str())
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("credentials", &"<redacted>")
            .finish()
    }
}

fn bad(msg: &str) -> Error {
    Error::Format(format!("s3 config: {msg}"))
}

async fn read_capped(resp: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    http::read_capped(resp, cap, "s3").await
}

fn validate_prefix_config(p: &str) -> Result<()> {
    if p.is_empty() {
        return Ok(());
    }
    let chars_ok = p
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'));
    if !chars_ok || p.split('/').any(|seg| matches!(seg, "" | "." | "..")) {
        return Err(bad("invalid prefix"));
    }
    Ok(())
}

enum Cond<'a> {
    IfNoneMatch,
    IfMatch(&'a Etag),
}

impl S3Backend {
    /// Validates the configuration and builds the HTTP client (no network I/O).
    pub fn new(cfg: S3Config) -> Result<Self> {
        let base = Url::parse(&cfg.endpoint).map_err(|_| bad("invalid endpoint URL"))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !matches!(base.path(), "" | "/")
        {
            return Err(bad("endpoint must be http(s)://host[:port] only"));
        }
        http::check_scheme(&base, cfg.allow_insecure_http).map_err(bad)?;
        if cfg.region.is_empty()
            || !cfg
                .region
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(bad("invalid region"));
        }
        if cfg.bucket.is_empty()
            || !cfg
                .bucket
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return Err(bad("invalid bucket"));
        }
        if cfg.access_key.is_empty() || cfg.secret_key.is_empty() {
            return Err(bad("missing credentials"));
        }
        // Virtual-host addressing puts the bucket in the DNS name and TLS certificate;
        // a dot would add a label that wildcard certificates (*.s3.example.com) do not
        // cover, so such buckets must use path-style addressing.
        if !cfg.path_style && cfg.bucket.contains('.') {
            return Err(bad("bucket names containing '.' require path_style"));
        }
        validate_prefix_config(&cfg.prefix)?;
        let client = http::build_client(cfg.extra_root_ca_pem.as_deref(), "s3")?;
        Ok(Self {
            client,
            base,
            region: cfg.region,
            bucket: cfg.bucket,
            prefix: cfg.prefix,
            access_key: cfg.access_key,
            secret_key: cfg.secret_key,
            path_style: cfg.path_style,
        })
    }

    fn full_key(&self, name: &str) -> String {
        if self.prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.prefix)
        }
    }

    /// Returns `(url, host header value, canonical uri)` for an object key (or the
    /// bucket itself when `key` is empty).
    fn locate(&self, key: &str, canonical_query: &str) -> Result<(Url, String, String)> {
        let host = self.base.host_str().unwrap_or_default();
        let (host, path) = if self.path_style {
            let p = if key.is_empty() {
                format!("/{}", self.bucket)
            } else {
                format!("/{}/{key}", self.bucket)
            };
            (host.to_string(), p)
        } else {
            (format!("{}.{host}", self.bucket), format!("/{key}"))
        };
        let authority = match self.base.port() {
            Some(p) => format!("{host}:{p}"),
            None => host,
        };
        let canonical_uri = sigv4::uri_encode(&path, true);
        let mut url = format!("{}://{authority}{canonical_uri}", self.base.scheme());
        if !canonical_query.is_empty() {
            url.push('?');
            url.push_str(canonical_query);
        }
        let url = Url::parse(&url).map_err(|_| bad("cannot build request URL"))?;
        Ok((url, authority, canonical_uri))
    }

    async fn send(
        &self,
        method: Method,
        key: &str,
        canonical_query: &str,
        cond: Option<(&str, String)>,
        body: &[u8],
    ) -> Result<reqwest::Response> {
        let (url, host, canonical_uri) = self.locate(key, canonical_query)?;
        let payload = sigv4::sha256_hex(body);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Io("system clock before 1970".into()))?
            .as_secs();
        let date = sigv4::amz_date(now);
        let mut signed: Vec<(&str, &str)> = vec![
            ("host", &host),
            ("x-amz-content-sha256", &payload),
            ("x-amz-date", &date),
        ];
        if let Some((k, v)) = &cond {
            signed.push((k, v));
        }
        let auth = sigv4::authorization(
            &self.access_key,
            &self.secret_key,
            &sigv4::SigningInput {
                method: method.as_str(),
                canonical_uri: &canonical_uri,
                canonical_query,
                headers: &signed,
                payload_sha256: &payload,
                amz_date: &date,
                region: &self.region,
                service: "s3",
            },
        );
        let mut req = self
            .client
            .request(method, url)
            .header("x-amz-date", &date)
            .header("x-amz-content-sha256", &payload)
            .header("authorization", auth);
        if let Some((k, v)) = cond {
            req = req.header(k, v);
        }
        if !body.is_empty() {
            req = req.body(body.to_vec());
        }
        req.send()
            .await
            .map_err(|e| Error::Io(format!("s3 request failed: {}", e.without_url())))
    }

    async fn put(&self, name: &str, data: &[u8], cond: Cond<'_>) -> Result<PutOutcome> {
        validate_name(name)?;
        let header = match cond {
            Cond::IfNoneMatch => ("if-none-match", "*".to_string()),
            Cond::IfMatch(e) => {
                if e.0.is_empty() || !e.0.bytes().all(|b| b.is_ascii_graphic() && b != b'"') {
                    return Err(Error::Format("invalid etag".into()));
                }
                ("if-match", format!("\"{}\"", e.0))
            }
        };
        let if_match = matches!(cond, Cond::IfMatch(_));
        let resp = self
            .send(Method::PUT, &self.full_key(name), "", Some(header), data)
            .await?;
        let status = resp.status();
        if status.is_success() {
            let etag = etag_of(&resp).ok_or_else(|| Error::Io("s3 put: missing ETag".into()))?;
            return Ok(PutOutcome::Written(etag));
        }
        let code = error_code(resp).await;
        match status {
            StatusCode::PRECONDITION_FAILED => Ok(PutOutcome::Conflict),
            StatusCode::CONFLICT if code.as_deref() == Some("ConditionalRequestConflict") => {
                Ok(PutOutcome::Conflict)
            }
            // If-Match on a missing object is a 404 on S3.
            StatusCode::NOT_FOUND if if_match && code.as_deref() != Some("NoSuchBucket") => {
                Ok(PutOutcome::Conflict)
            }
            StatusCode::NOT_IMPLEMENTED => Err(Error::Unsupported(
                "provider does not implement conditional writes".into(),
            )),
            _ => Err(http_error("put", status, code)),
        }
    }

    /// Checks that the provider honours conditional writes; call once at setup.
    ///
    /// Writes a random `probe/` object with `If-None-Match: *` twice (the second must
    /// be refused), tries an `If-Match` write with a wrong etag (must be refused) and
    /// removes the object again. Returns [`Error::Unsupported`] if a provider ignores
    /// the conditions.
    pub async fn probe_conditional_writes(&self) -> Result<()> {
        let id = data_encoding::HEXLOWER.encode(&rand::random::<[u8; 16]>());
        let name = format!("probe/{id}");
        let result = self.probe_inner(&name).await;
        let cleanup = self.delete(&name).await;
        result?;
        cleanup
    }

    async fn probe_inner(&self, name: &str) -> Result<()> {
        const IGNORED: &str = "provider ignores conditional writes";
        match self.put_if_absent(name, b"probe").await? {
            PutOutcome::Written(_) => {}
            PutOutcome::Conflict => {
                return Err(Error::Io("s3 probe object unexpectedly exists".into()));
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

fn etag_of(resp: &reqwest::Response) -> Option<Etag> {
    let v = resp.headers().get("etag")?.to_str().ok()?;
    let v = v.trim().trim_matches('"');
    (!v.is_empty()).then(|| Etag(v.to_string()))
}

fn http_error(op: &str, status: StatusCode, code: Option<String>) -> Error {
    match code {
        Some(c) => Error::Io(format!("s3 {op}: http {} {c}", status.as_u16())),
        None => Error::Io(format!("s3 {op}: http {}", status.as_u16())),
    }
}

/// Reads (a bounded part of) an error response and extracts the S3 `<Code>`.
async fn error_code(resp: reqwest::Response) -> Option<String> {
    let buf = read_capped(resp, MAX_META_BYTES).await.ok()?;
    let xml = String::from_utf8_lossy(&buf);
    let mut reader = Reader::from_str(&xml);
    let mut in_code = false;
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => in_code = e.local_name().as_ref() == b"Code",
            Event::Text(t) if in_code => {
                let s = t.decode().ok()?.trim().to_string();
                // Keep the code short and printable; it ends up in error messages.
                return (s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
                    .then_some(s);
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ListPage {
    keys: Vec<String>,
    truncated: bool,
    next_token: Option<String>,
}

fn parse_list(xml: &str) -> Result<ListPage> {
    let fmt_err = |e: &dyn fmt::Display| Error::Format(format!("s3 list response: {e}"));
    let mut reader = Reader::from_str(xml);
    let mut page = ListPage::default();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut text = String::new();
    loop {
        match reader.read_event().map_err(|e| fmt_err(&e))? {
            Event::Start(e) => {
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(fmt_err(&"nesting too deep"));
                }
                stack.push(e.local_name().as_ref().to_vec());
                text.clear();
            }
            Event::Text(t) => text.push_str(&t.decode().map_err(|e| fmt_err(&e))?),
            Event::GeneralRef(r) => {
                if let Some(c) = r.resolve_char_ref().map_err(|e| fmt_err(&e))? {
                    text.push(c);
                } else {
                    let name = r.decode().map_err(|e| fmt_err(&e))?;
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(s) => text.push_str(s),
                        None => return Err(fmt_err(&"unknown entity")),
                    }
                }
            }
            Event::End(_) => {
                let name = stack.pop().unwrap_or_default();
                let parent = stack.last().map(Vec::as_slice);
                match (name.as_slice(), parent) {
                    (b"Key", Some(b"Contents")) => page.keys.push(std::mem::take(&mut text)),
                    (b"IsTruncated", Some(b"ListBucketResult")) => {
                        page.truncated = text.trim() == "true"
                    }
                    (b"NextContinuationToken", Some(b"ListBucketResult")) => {
                        page.next_token = Some(std::mem::take(&mut text))
                    }
                    _ => {}
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(page)
}

#[async_trait::async_trait]
impl ContainerBackend for S3Backend {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        validate_name(name)?;
        let resp = self
            .send(Method::GET, &self.full_key(name), "", None, b"")
            .await?;
        let status = resp.status();
        if status.is_success() {
            let etag = etag_of(&resp).ok_or_else(|| Error::Io("s3 get: missing ETag".into()))?;
            let body = read_capped(resp, MAX_OBJECT_BYTES).await?;
            return Ok(Some((body, etag)));
        }
        let code = error_code(resp).await;
        if status == StatusCode::NOT_FOUND && code.as_deref() != Some("NoSuchBucket") {
            return Ok(None);
        }
        Err(http_error("get", status, code))
    }

    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        self.put(name, data, Cond::IfNoneMatch).await
    }

    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        self.put(name, data, Cond::IfMatch(etag)).await
    }

    async fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let resp = self
            .send(Method::DELETE, &self.full_key(name), "", None, b"")
            .await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let code = error_code(resp).await;
        if status == StatusCode::NOT_FOUND && code.as_deref() != Some("NoSuchBucket") {
            return Ok(());
        }
        Err(http_error("delete", status, code))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        validate_prefix(prefix)?;
        let key_prefix = self.full_key(prefix);
        let strip = key_prefix.len() - prefix.len();
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..MAX_LIST_PAGES {
            // Parameters sorted by name, as SigV4 requires.
            let mut query = String::new();
            if let Some(t) = &token {
                query.push_str(&format!(
                    "continuation-token={}&",
                    sigv4::uri_encode(t, false)
                ));
            }
            query.push_str(&format!(
                "list-type=2&prefix={}",
                sigv4::uri_encode(&key_prefix, false)
            ));
            let resp = self.send(Method::GET, "", &query, None, b"").await?;
            let status = resp.status();
            if !status.is_success() {
                let code = error_code(resp).await;
                return Err(http_error("list", status, code));
            }
            let body = read_capped(resp, MAX_META_BYTES).await?;
            let xml = std::str::from_utf8(&body)
                .map_err(|_| Error::Format("s3 list response is not UTF-8".into()))?;
            let page = parse_list(xml)?;
            for k in page.keys {
                if let Some(name) = k.get(strip..)
                    && k.starts_with(&key_prefix[..strip])
                    && validate_name(name).is_ok()
                {
                    out.push(name.to_string());
                }
            }
            if out.len() > MAX_LIST_KEYS {
                return Err(Error::Format("s3 list: too many objects".into()));
            }
            if !page.truncated {
                return Ok(out);
            }
            match page.next_token {
                // Every page must advance: a repeated token means a looping server.
                Some(t) if !t.is_empty() && seen.insert(t.clone()) => token = Some(t),
                _ => return Err(Error::Io("s3 list: truncated without usable token".into())),
            }
        }
        Err(Error::Io("s3 list: too many pages".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(path_style: bool, prefix: &str) -> S3Config {
        S3Config {
            endpoint: "https://s3.example.com".into(),
            region: "eu-central-1".into(),
            bucket: "vault".into(),
            prefix: prefix.into(),
            access_key: "AK".into(),
            secret_key: Zeroizing::new("super-secret".into()),
            path_style,
            allow_insecure_http: false,
            extra_root_ca_pem: None,
        }
    }

    #[test]
    fn url_styles() {
        let b = S3Backend::new(cfg(true, "a/b")).unwrap();
        let (u, h, p) = b.locate(&b.full_key("manifest"), "").unwrap();
        assert_eq!(u.as_str(), "https://s3.example.com/vault/a/b/manifest");
        assert_eq!(
            (h.as_str(), p.as_str()),
            ("s3.example.com", "/vault/a/b/manifest")
        );
        let b = S3Backend::new(cfg(false, "")).unwrap();
        let (u, h, _) = b.locate("manifest", "x=1").unwrap();
        assert_eq!(u.as_str(), "https://vault.s3.example.com/manifest?x=1");
        assert_eq!(h, "vault.s3.example.com");
        let (u, _, p) = b.locate("", "list-type=2").unwrap();
        assert_eq!(u.as_str(), "https://vault.s3.example.com/?list-type=2");
        assert_eq!(p, "/");
    }

    #[test]
    fn rejects_bad_config() {
        for p in ["/a", "a/", "a/../b", "a b", "a//b", "ä", ".."] {
            assert!(S3Backend::new(cfg(true, p)).is_err(), "{p:?}");
        }
        for p in ["", "a", "a/b-c_d.e"] {
            assert!(S3Backend::new(cfg(true, p)).is_ok(), "{p:?}");
        }
        let mut c = cfg(true, "");
        c.endpoint = "ftp://x".into();
        assert!(S3Backend::new(c).is_err());
        let mut c = cfg(true, "");
        c.endpoint = "https://u:p@x".into();
        assert!(S3Backend::new(c).is_err());
        let mut c = cfg(true, "");
        c.bucket = "a/b".into();
        assert!(S3Backend::new(c).is_err());
    }

    #[test]
    fn prefix_segments_must_be_plain() {
        for p in ["a/./b", "./a", "a/.", "a/.."] {
            assert!(S3Backend::new(cfg(true, p)).is_err(), "{p:?}");
        }
    }

    #[test]
    fn dotted_bucket_needs_path_style() {
        let mut c = cfg(false, "");
        c.bucket = "my.bucket".into();
        assert!(S3Backend::new(c).is_err());
        let mut c = cfg(true, "");
        c.bucket = "my.bucket".into();
        assert!(S3Backend::new(c).is_ok());
    }

    #[test]
    fn plain_http_only_for_loopback_or_opt_in() {
        let with = |ep: &str, allow: bool| {
            let mut c = cfg(true, "");
            c.endpoint = ep.into();
            c.allow_insecure_http = allow;
            S3Backend::new(c).is_ok()
        };
        assert!(with("https://s3.example.com", false));
        for ep in [
            "http://127.0.0.1:9000",
            "http://127.9.9.9",
            "http://localhost:9000",
            "http://[::1]:9000",
        ] {
            assert!(with(ep, false), "{ep}");
        }
        for ep in [
            "http://s3.example.com",
            "http://10.0.0.5:9000",
            "http://localhost.example.com",
        ] {
            assert!(!with(ep, false), "{ep}");
            assert!(with(ep, true), "{ep}");
        }
    }

    const TEST_CA: &str = "-----BEGIN CERTIFICATE-----
MIIBezCCASGgAwIBAgIUQl3NJ7mo9heL47xmUoov+BfWtpkwCgYIKoZIzj0EAwIw
EjEQMA4GA1UEAwwHdGVzdC1jYTAgFw0yNjExMTgxNjA5MzVaGA8yMTI2MTAyNTE2
MDkzNVowEjEQMA4GA1UEAwwHdGVzdC1jYTBZMBMGByqGSM49AgEGCCqGSM49AwEH
A0IABCTdcmXGGHWIuJdm55TudBoiEN5GlNVGzGGAfa2FUXS6dPmdDpen6NEsijY4
rFISDUhT9kPzTMTc+Bq2e/k20/ejUzBRMB0GA1UdDgQWBBR1UoNIqBDtoIXMGGjp
5x5YikNB6TAfBgNVHSMEGDAWgBR1UoNIqBDtoIXMGGjp5x5YikNB6TAPBgNVHRMB
Af8EBTADAQH/MAoGCCqGSM49BAMCA0gAMEUCIQDq291L/vuYEcayl8wWy0qvu8ZK
Cb5EjkYOPSa8MdI/aQIgfdEuDSRa1BmlEO2t2W2lrgTWDHTO36sl+jKWlvfdm8U=
-----END CERTIFICATE-----
";

    #[test]
    fn extra_root_ca_parsing() {
        let mut c = cfg(true, "");
        c.extra_root_ca_pem = Some(TEST_CA.into());
        assert!(S3Backend::new(c).is_ok());
        for bad_pem in [
            "",
            "not a pem",
            "-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n",
        ] {
            let mut c = cfg(true, "");
            c.extra_root_ca_pem = Some(bad_pem.into());
            assert!(S3Backend::new(c).is_err(), "{bad_pem:?}");
        }
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = cfg(true, "p");
        let s = format!("{c:?}");
        assert!(!s.contains("super-secret") && !s.contains("\"AK\""));
        let b = S3Backend::new(c).unwrap();
        let s = format!("{b:?}");
        assert!(!s.contains("super-secret") && !s.contains("AK"), "{s}");
    }

    #[test]
    fn parses_list_pages() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>p/seg/</Prefix>
<IsTruncated>true</IsTruncated><NextContinuationToken>a+b&amp;c=</NextContinuationToken>
<Contents><Key>p/seg/aa</Key><Size>1</Size></Contents><Contents><Key>p/seg/bb</Key></Contents></ListBucketResult>"#;
        let p = parse_list(xml).unwrap();
        assert_eq!(p.keys, vec!["p/seg/aa", "p/seg/bb"]);
        assert!(p.truncated);
        assert_eq!(p.next_token.as_deref(), Some("a+b&c="));
        assert!(parse_list("<ListBucketResult><Contents></ListBucketResult>").is_err());
    }

    #[test]
    fn list_nesting_depth_is_capped() {
        let nested = |n: usize| {
            format!(
                "<ListBucketResult>{}{}</ListBucketResult>",
                "<x>".repeat(n),
                "</x>".repeat(n)
            )
        };
        assert!(parse_list(&nested(MAX_XML_DEPTH - 1)).is_ok());
        assert!(matches!(
            parse_list(&nested(MAX_XML_DEPTH)),
            Err(Error::Format(_))
        ));
        assert!(parse_list(&nested(100_000)).is_err());
    }
}
