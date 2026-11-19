// SPDX-License-Identifier: MIT
//! What the client needs from its platform: the mailbox socket, a clock and a
//! random source.

use crate::error::{ClientError, perr};
use async_trait::async_trait;
use futures::future::LocalBoxFuture;

/// A relay mailbox WebSocket: one binary frame per Noise message.
#[async_trait(?Send)]
pub trait MailboxTransport {
    async fn send(&self, frame: &[u8]) -> Result<(), ClientError>;
    /// Next binary frame; `None` once the socket is closed.
    async fn recv(&self) -> Result<Option<Vec<u8>>, ClientError>;
    /// Close the socket (idempotent); pending and later `recv` calls end.
    fn close(&self);
}

/// Opens mailbox sockets (`wasm::WsFactory` in the browser,
/// `native::TokioFactory` natively).
#[async_trait(?Send)]
pub trait MailboxFactory {
    type Mailbox: MailboxTransport + 'static;
    /// Connect to `url` (`ws(s)://.../mailbox/<hex>`, see [`mailbox_url`]).
    async fn open_mailbox(&self, url: &str) -> Result<Self::Mailbox, ClientError>;
}

/// Wall clock and timers.
pub trait Clock {
    fn now_ms(&self) -> u64;
    /// Resolves after `ms` milliseconds.
    fn sleep(&self, ms: u64) -> LocalBoxFuture<'static, ()>;
}

/// Cryptographically secure random bytes.
pub trait Rng {
    fn fill(&self, buf: &mut [u8]);
}

/// The operating system's (or the browser's `crypto.getRandomValues`) random
/// source.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRng;

impl Rng for SystemRng {
    fn fill(&self, buf: &mut [u8]) {
        getrandom::getrandom(buf).expect("system random source unavailable");
    }
}

/// `{relay_origin}/mailbox/<32 hex>` with `https`/`http` mapped to `wss`/`ws`.
pub fn mailbox_url(relay_origin: &str, id: &[u8; 16]) -> Result<String, ClientError> {
    let (scheme, rest) = relay_origin
        .split_once("://")
        .ok_or_else(|| perr("relay url has no scheme"))?;
    let scheme = match scheme {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return Err(perr("relay url scheme")),
    };
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{scheme}://{}/mailbox/{hex}",
        rest.trim_end_matches('/')
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_urls() {
        let id = [0xab; 16];
        let hex = "ab".repeat(16);
        assert_eq!(
            mailbox_url("https://relay.example/", &id).unwrap(),
            format!("wss://relay.example/mailbox/{hex}")
        );
        assert_eq!(
            mailbox_url("http://127.0.0.1:8080", &id).unwrap(),
            format!("ws://127.0.0.1:8080/mailbox/{hex}")
        );
        assert!(mailbox_url("relay.example", &id).is_err());
        assert!(mailbox_url("ftp://relay.example", &id).is_err());
    }
}
