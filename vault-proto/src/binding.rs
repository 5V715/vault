// SPDX-License-Identifier: MIT
//! The app binding: which app (`app_id`) and which web origin a vault serves.
//!
//! A vault is bound to one `(app_id, origin)` pair when its container is created.
//! The browser declares both in the [`crate::PairOffer`]; the Noise prologue binds
//! them into the handshake, and the vault refuses an offer that does not match its
//! binding with [`crate::Notice::WrongApp`] before it asks the user.
//!
//! The origin is a declared, pinned label, not an authenticated claim: the page
//! supplies it (`location.origin` in a browser) and the vault cannot verify it.
//!
//! Rules (`docs/protocol/vault.md`, "App binding"):
//! - `app_id` matches `^[a-z0-9][a-z0-9.-]{0,63}$` (1 to 64 bytes).
//! - `origin` is `scheme "://" host [":" port]` with nothing after the authority
//!   (no path, not even `/`, no query, no fragment) and no userinfo. The scheme is
//!   `https`, or `http` only for the loopback hosts `localhost`, `127.0.0.1` and
//!   `[::1]`. The host is ASCII: a DNS name of letters, digits, `-` and `.` (an
//!   internationalized name must be given in punycode, `xn--...`), an IPv4
//!   address, or a bracketed IPv6 address.
//! - Canonical form (what offers, prologues and containers carry, compared byte for
//!   byte): scheme and host in lower case, the port without leading zeros, and the
//!   default port of the scheme (`:443` for https, `:80` for http) left out.
//!   [`normalize_origin`] produces it; [`validate_origin`] accepts only it.

use crate::ProtoError;
use alloc_prelude::*;

mod alloc_prelude {
    extern crate alloc;
    pub use alloc::{format, string::String};
}

/// Maximum `app_id` length in bytes.
pub const MAX_APP_ID: usize = 64;
/// Maximum origin length in bytes (scheme, a 253-byte host and a port fit).
pub const MAX_ORIGIN: usize = 300;

/// `app_id` must match `^[a-z0-9][a-z0-9.-]{0,63}$`.
pub fn validate_app_id(app_id: &str) -> Result<(), ProtoError> {
    let b = app_id.as_bytes();
    let first_ok = b
        .first()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = b
        .iter()
        .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-');
    if first_ok && rest_ok && b.len() <= MAX_APP_ID {
        Ok(())
    } else {
        Err(ProtoError::BadAppId)
    }
}

/// Accepts only an origin already in canonical form (see the module docs).
pub fn validate_origin(origin: &str) -> Result<(), ProtoError> {
    if normalize_origin(origin)? == origin {
        Ok(())
    } else {
        Err(ProtoError::BadOrigin("not in canonical form"))
    }
}

/// Checks `origin` against the rules and returns its canonical form.
pub fn normalize_origin(origin: &str) -> Result<String, ProtoError> {
    let bad = ProtoError::BadOrigin;
    if origin.len() > MAX_ORIGIN {
        return Err(bad("too long"));
    }
    if !origin.is_ascii() {
        return Err(bad("non-ASCII character (use punycode for the host)"));
    }
    if origin.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'\\') {
        return Err(bad("forbidden character"));
    }
    let (scheme, authority) = origin.split_once("://").ok_or(bad("missing scheme"))?;
    let scheme = scheme.to_ascii_lowercase();
    let (secure, default_port) = match scheme.as_str() {
        "https" => (true, 443u16),
        "http" => (false, 80u16),
        _ => return Err(bad("scheme must be https (or http for loopback)")),
    };
    if authority.contains(['/', '?', '#']) {
        return Err(bad("path, query or fragment"));
    }
    if authority.contains('@') {
        return Err(bad("userinfo"));
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, tail) = v6.split_once(']').ok_or(bad("bad IPv6 literal"))?;
        let port = match tail {
            "" => None,
            t => Some(t.strip_prefix(':').ok_or(bad("bad authority"))?),
        };
        if h.is_empty()
            || !h
                .bytes()
                .all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.')
        {
            return Err(bad("bad IPv6 literal"));
        }
        (format!("[{}]", h.to_ascii_lowercase()), port)
    } else {
        let (h, port) = match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        };
        let h = h.to_ascii_lowercase();
        let labels_ok = h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        });
        if h.is_empty() || h.len() > 253 || !labels_ok {
            return Err(bad("bad host"));
        }
        (h, port)
    };
    let port = match port {
        None => None,
        Some(p) => {
            let n = (!p.is_empty() && p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit()))
                .then(|| p.parse::<u16>().ok())
                .flatten()
                .filter(|&n| n != 0)
                .ok_or(bad("bad port"))?;
            (n != default_port).then_some(n)
        }
    };
    if !secure && !matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        return Err(bad(
            "http is only allowed for localhost, 127.0.0.1 and [::1]",
        ));
    }
    Ok(match port {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_rules() {
        for ok in [
            "a",
            "0",
            "signal-web",
            "org.example.app",
            "a-",
            &"a".repeat(64),
        ] {
            assert_eq!(validate_app_id(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "-a",
            ".a",
            "A",
            "Signal-web",
            "a_b",
            "a b",
            "a/b",
            "\u{e9}",
            &"a".repeat(65),
        ] {
            assert_eq!(validate_app_id(bad), Err(ProtoError::BadAppId), "{bad}");
        }
    }

    #[test]
    fn origin_normalization() {
        for (input, want) in [
            ("https://app.example", "https://app.example"),
            ("HTTPS://App.Example", "https://app.example"),
            ("https://app.example:443", "https://app.example"),
            ("https://app.example:0443", "https://app.example"),
            ("https://app.example:8443", "https://app.example:8443"),
            (
                "https://xn--bcher-kva.example",
                "https://xn--bcher-kva.example",
            ),
            ("https://192.0.2.1", "https://192.0.2.1"),
            ("https://[2001:DB8::1]:443", "https://[2001:db8::1]"),
            ("http://localhost:3000", "http://localhost:3000"),
            ("http://LOCALHOST:80", "http://localhost"),
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("http://[::1]:8080", "http://[::1]:8080"),
            ("https://app.example:80", "https://app.example:80"),
        ] {
            assert_eq!(normalize_origin(input).as_deref(), Ok(want), "{input}");
            assert_eq!(validate_origin(want), Ok(()), "{want}");
        }
        for bad in [
            "",
            "app.example",
            "ftp://app.example",
            "wss://app.example",
            "http://app.example",
            "http://192.168.1.2",
            "http://localhost.evil.example",
            "https://app.example/",
            "https://app.example/path",
            "https://app.example?q",
            "https://app.example#f",
            "https://user@app.example",
            "https://u:p@app.example",
            "https://",
            "https://:443",
            "https://app..example",
            "https://.app.example",
            "https://app.example.",
            "https://app_x.example",
            "https://b\u{fc}cher.example",
            "https://app.example:",
            "https://app.example:0",
            "https://app.example:65536",
            "https://app.example:1:2",
            "https://app.example:+1",
            "https://[]",
            "https://[::1",
            "https://[::1]x",
            "https://[g::1]",
            "https://app example",
            "https://app.example\\",
        ] {
            assert!(
                matches!(normalize_origin(bad), Err(ProtoError::BadOrigin(_))),
                "{bad}"
            );
        }
        let long = String::from("https://") + &"a".repeat(MAX_ORIGIN);
        assert!(normalize_origin(&long).is_err());
        // validate_origin accepts only the canonical form.
        for non_canonical in [
            "HTTPS://app.example",
            "https://App.example",
            "https://app.example:443",
            "http://localhost:80",
        ] {
            assert_eq!(
                validate_origin(non_canonical),
                Err(ProtoError::BadOrigin("not in canonical form")),
                "{non_canonical}"
            );
        }
    }
}
