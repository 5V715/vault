// SPDX-License-Identifier: MIT
//! Relay mailbox connection and the encrypted, chunked message channel on top of it.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use vault_proto::frame::MAX_PLAINTEXT;
use vault_proto::msg::validate_relay_url;
use vault_proto::noise::Session;
use vault_proto::{Chunk, Chunker, ErrorCode, PairOffer, ProtoError, Reassembler};
use zeroize::{Zeroize, Zeroizing};

use crate::ServerError;

pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest WebSocket frame/message accepted: one Noise transport message
/// (60 KiB plaintext + 16-byte tag), with a little slack.
const MAX_WS_FRAME: usize = MAX_PLAINTEXT + 1024;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// `{relay_url}/mailbox/<32 hex>`, with `https`/`http` mapped to `wss`/`ws`.
/// Plain `ws` is only accepted for loopback hosts (`validate_relay_url`).
pub(crate) fn mailbox_url(offer: &PairOffer) -> Result<String, ServerError> {
    validate_relay_url(&offer.relay_url).map_err(|_| ServerError::BadOffer)?;
    let (scheme, rest) = offer
        .relay_url
        .split_once("://")
        .ok_or(ServerError::BadOffer)?;
    let scheme = match scheme {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return Err(ServerError::BadOffer),
    };
    if rest.contains(['?', '#']) {
        return Err(ServerError::BadOffer);
    }
    let hex: String = offer
        .mailbox_id
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(format!(
        "{scheme}://{}/mailbox/{hex}",
        rest.trim_end_matches('/')
    ))
}

/// rustls client config: webpki roots plus an optional extra CA bundle (PEM),
/// ring provider, safe default protocol versions.
pub(crate) fn tls_config(extra_ca_pem: Option<&[u8]>) -> Result<rustls::ClientConfig, ServerError> {
    use rustls_pki_types::{CertificateDer, pem::PemObject};
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_ca_pem {
        let mut added = 0usize;
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert = cert.map_err(|_| ServerError::Tls("invalid extra CA PEM"))?;
            roots
                .add(cert)
                .map_err(|_| ServerError::Tls("invalid extra CA certificate"))?;
            added += 1;
        }
        if added == 0 {
            return Err(ServerError::Tls("extra CA PEM holds no certificate"));
        }
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| ServerError::Tls("no usable TLS protocol version"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(cfg)
}

/// Dial the relay mailbox. No `Origin` header is sent (native client).
pub(crate) async fn connect(url: &str, tls: Arc<rustls::ClientConfig>) -> Result<Ws, ServerError> {
    let req = url
        .into_client_request()
        .map_err(|_| ServerError::BadOffer)?;
    let connector = if url.starts_with("wss://") {
        Connector::Rustls(tls)
    } else {
        Connector::Plain
    };
    let ws_cfg = WebSocketConfig::default()
        .max_frame_size(Some(MAX_WS_FRAME))
        .max_message_size(Some(MAX_WS_FRAME));
    let (ws, _) =
        tokio_tungstenite::connect_async_tls_with_config(req, Some(ws_cfg), true, Some(connector))
            .await
            .map_err(|e| match &e {
                tokio_tungstenite::tungstenite::Error::Http(r) if r.status().as_u16() == 409 => {
                    ServerError::MailboxTaken
                }
                _ => ServerError::Relay(relay_error(&e)),
            })?;
    Ok(ws)
}

fn relay_error(e: &tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error as E;
    match e {
        E::Http(r) => format!("relay refused: HTTP {}", r.status().as_u16()),
        E::Io(e) => format!("connect failed: {}", e.kind()),
        E::Tls(_) => "TLS failure".into(),
        _ => "websocket handshake failed".into(),
    }
}

/// Next binary frame during the Noise handshake.
pub(crate) async fn recv_raw(ws: &mut Ws) -> Result<Vec<u8>, ServerError> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => return Ok(b.to_vec()),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(_)) | Some(Err(_)) | None => return Err(ServerError::Handshake),
        }
    }
}

pub(crate) async fn send_raw(ws: &mut Ws, frame: Vec<u8>) -> Result<(), ServerError> {
    ws.send(Message::Binary(frame.into()))
        .await
        .map_err(|_| ServerError::Handshake)
}

/// Why the channel stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelError {
    /// The peer or relay closed the connection, or it failed / timed out.
    Disconnected,
    /// Undecryptable frame, bad chunk sequence, oversize message or non-binary
    /// frame. Carries the error code to report (with request id 0) when the
    /// Noise session is still usable, `None` when it is not.
    Protocol(Option<ErrorCode>),
}

fn code_of(e: ProtoError) -> ErrorCode {
    match e {
        ProtoError::TooLarge | ProtoError::TooManyChunks => ErrorCode::TooLarge,
        _ => ErrorCode::BadRequest,
    }
}

/// Noise transport over the mailbox WebSocket, one application message at a time.
pub(crate) struct Channel {
    ws: Ws,
    noise: Session,
    chunker: Chunker,
    reasm: Reassembler,
    send_timeout: Duration,
}

impl Channel {
    pub(crate) fn new(ws: Ws, noise: Session, send_timeout: Duration) -> Self {
        Self {
            ws,
            noise,
            chunker: Chunker::new(),
            reasm: Reassembler::new(),
            send_timeout,
        }
    }

    /// Chunk, encrypt and send one application message.
    pub(crate) async fn send(&mut self, msg: &[u8]) -> Result<(), ChannelError> {
        let chunks = self
            .chunker
            .split(msg)
            .map_err(|_| ChannelError::Protocol(None))?;
        let fut = async {
            for mut c in chunks {
                let pt = Zeroizing::new(c.encode());
                c.payload.zeroize();
                let ct = self
                    .noise
                    .encrypt(&pt)
                    .map_err(|_| ChannelError::Protocol(None))?;
                self.ws
                    .feed(Message::Binary(ct.into()))
                    .await
                    .map_err(|_| ChannelError::Disconnected)?;
            }
            self.ws
                .flush()
                .await
                .map_err(|_| ChannelError::Disconnected)
        };
        tokio::time::timeout(self.send_timeout, fut)
            .await
            .unwrap_or(Err(ChannelError::Disconnected))
    }

    /// Receive one reassembled application message.
    ///
    /// Cancel-safe: the only await point is the next WebSocket frame, and the
    /// reassembly state lives in `self`.
    pub(crate) async fn recv(&mut self) -> Result<Zeroizing<Vec<u8>>, ChannelError> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Binary(b))) => {
                    let pt = Zeroizing::new(
                        self.noise
                            .decrypt(&b)
                            .map_err(|_| ChannelError::Protocol(None))?,
                    );
                    let chunk =
                        Chunk::decode(&pt).map_err(|e| ChannelError::Protocol(Some(code_of(e))))?;
                    if let Some(m) = self
                        .reasm
                        .push(chunk)
                        .map_err(|e| ChannelError::Protocol(Some(code_of(e))))?
                    {
                        return Ok(Zeroizing::new(m));
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Text(_))) => {
                    return Err(ChannelError::Protocol(Some(ErrorCode::BadRequest)));
                }
                Some(Ok(Message::Close(_))) | None => return Err(ChannelError::Disconnected),
                Some(Err(e)) => {
                    use tokio_tungstenite::tungstenite::Error as E;
                    return Err(match e {
                        E::Capacity(_) | E::Protocol(_) => ChannelError::Protocol(None),
                        _ => ChannelError::Disconnected,
                    });
                }
            }
        }
    }

    /// Send a close frame (best effort) and drop the connection.
    pub(crate) async fn close(mut self) {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.ws.close(None)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(url: &str) -> PairOffer {
        PairOffer {
            version: vault_proto::PROTOCOL_VERSION,
            relay_url: url.into(),
            mailbox_id: [0xab; 16],
            browser_eph_pub: [1; 32],
            app_id: "test-app".into(),
            origin: "https://app.example".into(),
        }
    }

    #[test]
    fn mailbox_urls() {
        let id = "ab".repeat(16);
        for (url, want) in [
            (
                "https://relay.example",
                format!("wss://relay.example/mailbox/{id}"),
            ),
            (
                "wss://relay.example/",
                format!("wss://relay.example/mailbox/{id}"),
            ),
            (
                "https://relay.example:8443/base/",
                format!("wss://relay.example:8443/base/mailbox/{id}"),
            ),
            (
                "ws://127.0.0.1:9000",
                format!("ws://127.0.0.1:9000/mailbox/{id}"),
            ),
            ("http://localhost/", format!("ws://localhost/mailbox/{id}")),
        ] {
            assert_eq!(mailbox_url(&offer(url)).unwrap(), want, "{url}");
        }
        for bad in [
            "ws://relay.example",
            "http://10.0.0.1",
            "ftp://relay.example",
            "https://relay.example/?x=1",
            "https://relay.example/#f",
        ] {
            assert!(mailbox_url(&offer(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn extra_ca_must_parse() {
        assert!(tls_config(None).is_ok());
        assert!(tls_config(Some(b"not a pem")).is_err());
        let bad = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        assert!(tls_config(Some(bad)).is_err());
    }
}
