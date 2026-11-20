// SPDX-License-Identifier: MIT
//! Server-side TLS configuration for [`crate::serve_tls`]: rustls with the ring
//! provider only, TLS 1.2 and 1.3, no client authentication, HTTP/1.1 only.

use std::sync::Arc;

use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

/// Builds the rustls server config from PEM text: a certificate chain (leaf
/// first; intermediates may follow) and its private key (PKCS#8, PKCS#1 or SEC1).
/// The error is a short message that never contains key material.
pub fn tls_server_config(cert_pem: &[u8], key_pem: &[u8]) -> Result<Arc<ServerConfig>, String> {
    let chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "the certificate file is not valid PEM".to_string())?;
    if chain.is_empty() {
        return Err("no certificate found in the certificate file".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|_| "no private key found in the key file".to_string())?;
    let mut cfg =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| "no usable TLS protocol version".to_string())?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| {
                format!("the private key does not match the certificate or is unusable: {e}")
            })?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}
