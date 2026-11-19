//! Application messages and the QR pair offer.

use crate::ProtoError;
use alloc_prelude::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

mod alloc_prelude {
    extern crate alloc;
    pub use alloc::{string::String, vec::Vec};
}

/// Prefix of the QR payload.
pub const QR_PREFIX: &str = "signal-web-pair:v1:";
/// Maximum relay URL length in bytes.
pub const MAX_RELAY_URL: usize = 512;
/// Maximum `device_name` length in bytes (UTF-8, no control characters).
pub const MAX_DEVICE_NAME: usize = 64;
/// Maximum key or prefix length in bytes (1 KiB).
pub const MAX_KEY: usize = 1024;
/// Maximum single value length in bytes (16 MiB).
pub const MAX_VALUE: usize = 16 * 1024 * 1024;
/// Maximum total key+value bytes in one `Entries` response (32 MiB).
pub const MAX_ENTRIES_TOTAL: usize = crate::frame::MAX_MESSAGE;

/// Decode one postcard value and require that no bytes remain.
fn decode_exact<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    let (v, rest) = postcard::take_from_bytes(bytes).map_err(|_| ProtoError::Malformed)?;
    if rest.is_empty() {
        Ok(v)
    } else {
        Err(ProtoError::Malformed)
    }
}

fn check_len(len: usize, max: usize) -> Result<(), ProtoError> {
    if len > max {
        Err(ProtoError::TooLarge)
    } else {
        Ok(())
    }
}

/// What the browser shows as a QR code and the vault scans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairOffer {
    pub relay_url: String,
    pub mailbox_id: [u8; 16],
    pub browser_eph_pub: [u8; 32],
}

impl PairOffer {
    /// Validate and encode as `signal-web-pair:v1:<base64url(postcard)>`.
    pub fn to_qr(&self) -> Result<String, ProtoError> {
        validate_relay_url(&self.relay_url)?;
        let bytes = postcard::to_allocvec(self).map_err(|_| ProtoError::Malformed)?;
        let mut s = String::from(QR_PREFIX);
        s.push_str(&URL_SAFE_NO_PAD.encode(bytes));
        Ok(s)
    }

    /// Parse and validate a scanned QR payload.
    pub fn from_qr(s: &str) -> Result<Self, ProtoError> {
        let b64 = s.strip_prefix(QR_PREFIX).ok_or(ProtoError::BadQrPrefix)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(b64)
            .map_err(|_| ProtoError::BadQrEncoding)?;
        let offer: PairOffer = decode_exact(&bytes)?;
        validate_relay_url(&offer.relay_url)?;
        Ok(offer)
    }
}

/// https/wss anywhere; http/ws only for loopback hosts. At most 512 bytes.
pub fn validate_relay_url(url: &str) -> Result<(), ProtoError> {
    if url.len() > MAX_RELAY_URL {
        return Err(ProtoError::BadRelayUrl("too long"));
    }
    if url.bytes().any(|b| b <= b' ' || b >= 0x7f || b == b'\\') {
        return Err(ProtoError::BadRelayUrl("forbidden character"));
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or(ProtoError::BadRelayUrl("missing scheme"))?;
    let secure = match scheme {
        "https" | "wss" => true,
        "http" | "ws" => false,
        _ => return Err(ProtoError::BadRelayUrl("unsupported scheme")),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.bytes().any(|b| b <= b' ') {
        return Err(ProtoError::BadRelayUrl("bad authority"));
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, tail) = v6
            .split_once(']')
            .ok_or(ProtoError::BadRelayUrl("bad authority"))?;
        match tail {
            "" => (h, None),
            t => (
                h,
                Some(
                    t.strip_prefix(':')
                        .ok_or(ProtoError::BadRelayUrl("bad authority"))?,
                ),
            ),
        }
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if let Some(p) = port {
        let valid = !p.is_empty()
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u16>().is_ok_and(|n| n != 0);
        if !valid {
            return Err(ProtoError::BadRelayUrl("bad port"));
        }
    }
    if host.is_empty() {
        return Err(ProtoError::BadRelayUrl("empty host"));
    }
    if !secure && !is_loopback(host) {
        return Err(ProtoError::BadRelayUrl(
            "insecure scheme for non-loopback host",
        ));
    }
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// Error codes carried by `ResponseBody::Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    NotApproved,
    Conflict,
    Storage,
    TooLarge,
    BadRequest,
    Locked,
    Internal,
}

/// Browser to vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub body: RequestBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestBody {
    OpenSession {
        device_name: String,
    },
    Get {
        key: Vec<u8>,
    },
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    PutIfAbsent {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        key: Vec<u8>,
    },
    ScanPrefix {
        prefix: Vec<u8>,
    },
    Commit,
    Clear,
    Close,
    /// Keepalive: answered `Ok`, allowed in any state (also before approval),
    /// touches no data. Appended last so earlier variant tags stay stable.
    Ping,
}

/// Vault to browser, answering the request with the same `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub body: ResponseBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseBody {
    Ok,
    Value(Option<Vec<u8>>),
    Bool(bool),
    Entries(Vec<(Vec<u8>, Vec<u8>)>),
    Committed { seq: u64 },
    Approved,
    Denied,
    Error { code: ErrorCode, msg: String },
}

/// Unsolicited vault to browser messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notice {
    SessionTerminated { reason: String },
    Locked,
}

/// Top-level frame content, vault to browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VaultMessage {
    Response(Response),
    Notice(Notice),
}

macro_rules! codec {
    ($t:ty) => {
        impl $t {
            pub fn encode(&self) -> Vec<u8> {
                postcard::to_allocvec(self).expect("postcard encoding to Vec cannot fail")
            }
            /// Decode exactly one value (trailing bytes are an error) and enforce field caps.
            pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
                let v: Self = decode_exact(bytes)?;
                v.validate()?;
                Ok(v)
            }
        }
    };
}

impl Request {
    fn validate(&self) -> Result<(), ProtoError> {
        match &self.body {
            RequestBody::OpenSession { device_name } => {
                check_len(device_name.len(), MAX_DEVICE_NAME)?;
                if device_name.chars().any(char::is_control) {
                    return Err(ProtoError::BadRequest);
                }
            }
            RequestBody::Get { key } | RequestBody::Delete { key } => {
                check_len(key.len(), MAX_KEY)?
            }
            RequestBody::ScanPrefix { prefix } => check_len(prefix.len(), MAX_KEY)?,
            RequestBody::Put { key, value } | RequestBody::PutIfAbsent { key, value } => {
                check_len(key.len(), MAX_KEY)?;
                check_len(value.len(), MAX_VALUE)?;
            }
            RequestBody::Commit | RequestBody::Clear | RequestBody::Close | RequestBody::Ping => {}
        }
        Ok(())
    }
}

impl Response {
    fn validate(&self) -> Result<(), ProtoError> {
        match &self.body {
            ResponseBody::Value(Some(v)) => check_len(v.len(), MAX_VALUE)?,
            ResponseBody::Entries(es) => {
                let mut total = 0usize;
                for (k, v) in es {
                    check_len(k.len(), MAX_KEY)?;
                    check_len(v.len(), MAX_VALUE)?;
                    total = total.saturating_add(k.len() + v.len());
                    check_len(total, MAX_ENTRIES_TOTAL)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Notice {
    fn validate(&self) -> Result<(), ProtoError> {
        Ok(())
    }
}

impl VaultMessage {
    fn validate(&self) -> Result<(), ProtoError> {
        match self {
            VaultMessage::Response(r) => r.validate(),
            VaultMessage::Notice(n) => n.validate(),
        }
    }
}

codec!(Request);
codec!(Response);
codec!(Notice);
codec!(VaultMessage);

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(url: &str) -> PairOffer {
        PairOffer {
            relay_url: url.into(),
            mailbox_id: [7; 16],
            browser_eph_pub: [9; 32],
        }
    }

    #[test]
    fn qr_roundtrip_and_rejects_bad_prefix() {
        let o = offer("wss://relay.example/mailbox");
        let qr = o.to_qr().unwrap();
        assert!(qr.starts_with("signal-web-pair:v1:"));
        assert_eq!(PairOffer::from_qr(&qr).unwrap(), o);
        let bad = qr.replacen("v1", "v2", 1);
        assert_eq!(PairOffer::from_qr(&bad), Err(ProtoError::BadQrPrefix));
        assert_eq!(PairOffer::from_qr("hello"), Err(ProtoError::BadQrPrefix));
        assert_eq!(
            PairOffer::from_qr("signal-web-pair:v1:!!!"),
            Err(ProtoError::BadQrEncoding)
        );
        assert_eq!(
            PairOffer::from_qr("signal-web-pair:v1:AAAA"),
            Err(ProtoError::Malformed)
        );
    }

    #[test]
    fn relay_url_validation() {
        for ok in [
            "https://r.example",
            "wss://r.example:8443/x",
            "ws://localhost:8080",
            "http://127.0.0.1:3000",
            "ws://[::1]:3000",
        ] {
            assert!(validate_relay_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://r.example",
            "ws://192.168.1.2",
            "ftp://r.example",
            "r.example",
            "https://",
            "https://user@r.example",
            "http://localhost.evil.example",
        ] {
            assert!(validate_relay_url(bad).is_err(), "{bad}");
        }
        let long = alloc_prelude::String::from("https://") + &"a".repeat(MAX_RELAY_URL);
        assert!(validate_relay_url(&long).is_err());
        assert!(offer("http://r.example").to_qr().is_err());
        // a tampered url inside a well-formed payload is rejected on parse
        let raw = postcard::to_allocvec(&offer("http://r.example")).unwrap();
        let qr = alloc_prelude::String::from(QR_PREFIX) + &URL_SAFE_NO_PAD.encode(raw);
        assert!(PairOffer::from_qr(&qr).is_err());
    }

    #[test]
    fn messages_roundtrip() {
        let reqs = [
            RequestBody::OpenSession {
                device_name: "Firefox".into(),
            },
            RequestBody::Get { key: vec![1] },
            RequestBody::Put {
                key: vec![1],
                value: vec![2, 3],
            },
            RequestBody::PutIfAbsent {
                key: vec![],
                value: vec![],
            },
            RequestBody::Delete { key: vec![4] },
            RequestBody::ScanPrefix { prefix: vec![5] },
            RequestBody::Commit,
            RequestBody::Clear,
            RequestBody::Close,
            RequestBody::Ping,
        ];
        for (i, body) in reqs.into_iter().enumerate() {
            let r = Request {
                id: i as u64 * 1_000_000_007,
                body,
            };
            assert_eq!(Request::decode(&r.encode()).unwrap(), r);
        }
        let resps = [
            ResponseBody::Ok,
            ResponseBody::Value(None),
            ResponseBody::Value(Some(vec![1, 2])),
            ResponseBody::Bool(true),
            ResponseBody::Entries(vec![(vec![1], vec![2]), (vec![], vec![])]),
            ResponseBody::Committed { seq: u64::MAX },
            ResponseBody::Approved,
            ResponseBody::Denied,
            ResponseBody::Error {
                code: ErrorCode::Conflict,
                msg: "x".into(),
            },
        ];
        for (i, body) in resps.into_iter().enumerate() {
            let r = Response { id: i as u64, body };
            assert_eq!(Response::decode(&r.encode()).unwrap(), r);
            let m = VaultMessage::Response(r);
            assert_eq!(VaultMessage::decode(&m.encode()).unwrap(), m);
        }
        for n in [
            Notice::Locked,
            Notice::SessionTerminated {
                reason: "replaced".into(),
            },
        ] {
            assert_eq!(Notice::decode(&n.encode()).unwrap(), n);
            let m = VaultMessage::Notice(n);
            assert_eq!(VaultMessage::decode(&m.encode()).unwrap(), m);
        }
        assert!(Request::decode(&[0xff, 0xff]).is_err());
    }

    #[test]
    fn ping_wire_tag_is_pinned() {
        // id 5, variant tag 9: Ping is appended after Close (tag 8).
        let b = Request {
            id: 5,
            body: RequestBody::Ping,
        }
        .encode();
        assert_eq!(b, [5, 9]);
        let c = Request {
            id: 5,
            body: RequestBody::Close,
        }
        .encode();
        assert_eq!(c, [5, 8]);
    }

    #[test]
    fn rejects_trailing_bytes() {
        let o = offer("wss://r.example");
        let mut raw = postcard::to_allocvec(&o).unwrap();
        raw.push(0);
        let qr = alloc_prelude::String::from(QR_PREFIX) + &URL_SAFE_NO_PAD.encode(raw);
        assert_eq!(PairOffer::from_qr(&qr), Err(ProtoError::Malformed));

        let mut b = Request {
            id: 1,
            body: RequestBody::Commit,
        }
        .encode();
        b.push(0);
        assert!(Request::decode(&b).is_err());
        let mut b = Response {
            id: 1,
            body: ResponseBody::Ok,
        }
        .encode();
        b.push(0);
        assert!(Response::decode(&b).is_err());
        let mut b = Notice::Locked.encode();
        b.push(0);
        assert!(Notice::decode(&b).is_err());
        let mut b = VaultMessage::Notice(Notice::Locked).encode();
        b.push(0);
        assert!(VaultMessage::decode(&b).is_err());
    }

    #[test]
    fn field_caps_on_decode() {
        let req = |body| Request { id: 1, body }.encode();
        let ok = |b: &[u8]| Request::decode(b).is_ok();
        // device_name
        assert!(ok(&req(RequestBody::OpenSession {
            device_name: "a".repeat(MAX_DEVICE_NAME)
        })));
        assert_eq!(
            Request::decode(&req(RequestBody::OpenSession {
                device_name: "a".repeat(MAX_DEVICE_NAME + 1)
            })),
            Err(ProtoError::TooLarge)
        );
        assert!(
            Request::decode(&req(RequestBody::OpenSession {
                device_name: "a\nb".into()
            }))
            .is_err()
        );
        assert!(
            Request::decode(&req(RequestBody::OpenSession {
                device_name: "a\u{7}".into()
            }))
            .is_err()
        );
        // keys and prefixes
        assert!(ok(&req(RequestBody::Get {
            key: vec![0; MAX_KEY]
        })));
        for body in [
            RequestBody::Get {
                key: vec![0; MAX_KEY + 1],
            },
            RequestBody::Put {
                key: vec![0; MAX_KEY + 1],
                value: vec![],
            },
            RequestBody::PutIfAbsent {
                key: vec![0; MAX_KEY + 1],
                value: vec![],
            },
            RequestBody::Delete {
                key: vec![0; MAX_KEY + 1],
            },
            RequestBody::ScanPrefix {
                prefix: vec![0; MAX_KEY + 1],
            },
        ] {
            assert_eq!(Request::decode(&req(body)), Err(ProtoError::TooLarge));
        }
        // values
        assert!(ok(&req(RequestBody::Put {
            key: vec![],
            value: vec![0; MAX_VALUE]
        })));
        assert_eq!(
            Request::decode(&req(RequestBody::Put {
                key: vec![],
                value: vec![0; MAX_VALUE + 1]
            })),
            Err(ProtoError::TooLarge)
        );
        // responses
        let resp = |body| Response { id: 1, body }.encode();
        assert!(Response::decode(&resp(ResponseBody::Value(Some(vec![0; MAX_VALUE])))).is_ok());
        assert_eq!(
            Response::decode(&resp(ResponseBody::Value(Some(vec![0; MAX_VALUE + 1])))),
            Err(ProtoError::TooLarge)
        );
        let entry = |n| (vec![0u8; 8], vec![0u8; n]);
        // three entries of 12 MiB each exceed the 32 MiB total though each is under 16 MiB
        let big = ResponseBody::Entries(vec![entry(12 << 20), entry(12 << 20), entry(12 << 20)]);
        assert_eq!(Response::decode(&resp(big)), Err(ProtoError::TooLarge));
        let fine = ResponseBody::Entries(vec![entry(12 << 20), entry(12 << 20)]);
        assert!(Response::decode(&resp(fine)).is_ok());
        let bad_key = ResponseBody::Entries(vec![(vec![0; MAX_KEY + 1], vec![])]);
        assert_eq!(Response::decode(&resp(bad_key)), Err(ProtoError::TooLarge));
    }

    #[test]
    fn relay_url_strictness() {
        for bad in [
            "https://r.example\\x",
            "https://r.example/\u{e9}",
            "https://r.example/\u{7f}",
            "https://r.example:abc",
            "https://r.example:",
            "https://r.example:0",
            "https://r.example:65536",
            "https://r.example:1:2",
            "https://[::1]:99999",
            "https://u:p@r.example",
        ] {
            assert!(validate_relay_url(bad).is_err(), "{bad}");
        }
        for ok in [
            "https://r.example:65535",
            "https://r.example:1/x",
            "wss://[::1]:443",
        ] {
            assert!(validate_relay_url(ok).is_ok(), "{ok}");
        }
    }
}
