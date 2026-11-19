// SPDX-License-Identifier: MIT

//! HTTP plumbing shared by the network backends (S3, WebDAV): body caps, the
//! plain-http policy, custom root CAs and the hardened client.

use std::time::Duration;

use reqwest::Url;

use crate::{Error, Result};

/// Request timeout; backends never retry (the replication layer owns retry policy).
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on entries returned by one `list` call.
pub(crate) const MAX_LIST_KEYS: usize = 100_000;
/// Upper bound on an object download (segments are far smaller).
pub(crate) const MAX_OBJECT_BYTES: usize = 64 << 20;
/// Upper bound on list and error response bodies.
pub(crate) const MAX_META_BYTES: usize = 16 << 20;

pub(crate) fn is_loopback_host(u: &Url) -> bool {
    let Some(h) = u.host_str() else { return false };
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    h.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|a| a.is_loopback())
}

/// Enforces the plain-http policy: `http://` only for loopback hosts unless
/// `allow_insecure_http` is set; any other scheme is refused.
pub(crate) fn check_scheme(
    u: &Url,
    allow_insecure_http: bool,
) -> std::result::Result<(), &'static str> {
    match u.scheme() {
        "https" => Ok(()),
        "http" if allow_insecure_http || is_loopback_host(u) => Ok(()),
        "http" => Err("plain http is only allowed for loopback hosts (set allow_insecure_http)"),
        _ => Err("URL must be http(s)"),
    }
}

fn parse_root_cas(pem: &str, label: &str) -> Result<Vec<reqwest::Certificate>> {
    let bad = |m: &str| Error::Format(format!("{label} config: {m}"));
    let certs = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
        .map_err(|_| bad("invalid extra_root_ca_pem"))?;
    if certs.is_empty() {
        return Err(bad("extra_root_ca_pem contains no certificate"));
    }
    Ok(certs)
}

/// Builds the HTTP client: 30 s timeout, redirects disabled, optional extra root CAs.
pub(crate) fn build_client(
    extra_root_ca_pem: Option<&str>,
    label: &str,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(pem) = extra_root_ca_pem {
        for cert in parse_root_cas(pem, label)? {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder
        .build()
        .map_err(|e| Error::Io(format!("{label} client: {}", e.without_url())))
}

/// Reads a response body, failing once it exceeds `cap` bytes.
pub(crate) async fn read_capped(
    mut resp: reqwest::Response,
    cap: usize,
    label: &str,
) -> Result<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > cap as u64) {
        return Err(Error::Io(format!("{label} response body too large")));
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| Error::Io(format!("{label} response body: {}", e.without_url())))?
    {
        if buf.len() + chunk.len() > cap {
            return Err(Error::Io(format!("{label} response body too large")));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}
