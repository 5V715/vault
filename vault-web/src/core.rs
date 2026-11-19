// SPDX-License-Identifier: MIT
//! The platform-independent parts of the package: state and close-reason
//! names, the choice of the page origin, and the QR module matrix. Unit-tested
//! on the host.

use qrcode::{Color, EcLevel, QrCode};
use vault_client::{CloseReason, SessionState};

/// The JS name of a session state (`onState`'s first argument).
pub fn state_name(s: &SessionState) -> &'static str {
    match s {
        SessionState::Pairing => "pairing",
        SessionState::AwaitingApproval => "awaitingApproval",
        SessionState::Active => "active",
        SessionState::Closed(_) => "closed",
    }
}

/// The JS name of a close reason: vault-client's variant name in camelCase
/// (`onState`'s second argument). The variant's text, with any detail, is
/// `reason.to_string()` (the third argument).
pub fn reason_name(r: &CloseReason) -> &'static str {
    match r {
        CloseReason::Denied => "denied",
        CloseReason::Locked => "locked",
        CloseReason::Terminated(_) => "terminated",
        CloseReason::Disconnected => "disconnected",
        CloseReason::Cancelled => "cancelled",
        CloseReason::CommitFailed => "commitFailed",
        CloseReason::SendFailed => "sendFailed",
        CloseReason::ProtocolError => "protocolError",
        CloseReason::Closed => "closed",
        CloseReason::RelayUnreachable(_) => "relayUnreachable",
        CloseReason::PairingFailed(_) => "pairingFailed",
        CloseReason::KeepaliveFailed(_) => "keepaliveFailed",
        CloseReason::KeepaliveTimedOut => "keepaliveTimedOut",
        CloseReason::Failed(_) => "failed",
        CloseReason::WrongApp => "wrongApp",
    }
}

/// The origin the page declares to the vault.
///
/// In a browser (`location` is the page's `location.origin`) it is always the
/// page origin; `test_origin` is refused unless it names the same origin. Only
/// runtimes without `location` (Node tests) take `test_origin`. Both are
/// normalized (lower-case host, default port dropped) and checked against the
/// origin rules (`https`, `http` only for loopback, no path).
pub fn resolve_origin(location: Option<&str>, test_origin: Option<&str>) -> Result<String, String> {
    let norm = |o: &str| vault_proto::binding::normalize_origin(o).map_err(|e| e.to_string());
    match (location, test_origin) {
        (Some(loc), None) => norm(loc),
        (Some(loc), Some(t)) => {
            let (loc, t) = (norm(loc)?, norm(t)?);
            if loc == t {
                Ok(loc)
            } else {
                Err(format!(
                    "testOrigin is only for runtimes without location (tests); \
                     in a browser the origin is location.origin ({loc}), not {t}"
                ))
            }
        }
        (None, Some(t)) => norm(t),
        (None, None) => Err("no page origin: location.origin is missing \
             (outside a browser, pass testOrigin)"
            .into()),
    }
}

/// A QR code as a square module matrix, without quiet zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    /// Modules per side (`17 + 4 * version`).
    pub size: usize,
    /// `size * size` bytes, row by row from the top left; 1 is dark, 0 light.
    pub modules: Vec<u8>,
}

/// Encode `text` as a QR code (error correction level M, the encoder's optimal
/// segments, smallest version that fits). Renderers add a light quiet zone of
/// 4 modules on every side.
pub fn qr_matrix(text: &str) -> Result<QrMatrix, String> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M)
        .map_err(|e| format!("cannot encode the QR code: {e}"))?;
    let size = code.width();
    let mut modules = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            modules.push(u8::from(code[(x, y)] == Color::Dark));
        }
    }
    Ok(QrMatrix { size, modules })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared with the Node e2e test, which checks the wasm build against it.
    const VECTOR: &str = include_str!("../tests/qr-vector.txt");

    struct Vector {
        text: String,
        size: usize,
        rows: Vec<String>,
    }

    /// `text: ...`, `size: N`, then N rows of `#` (dark) and `.` (light).
    fn parse_vector() -> Vector {
        let mut lines = VECTOR.lines().filter(|l| !l.starts_with("//"));
        let text = lines
            .next()
            .unwrap()
            .strip_prefix("text: ")
            .unwrap()
            .to_string();
        let size = lines
            .next()
            .unwrap()
            .strip_prefix("size: ")
            .unwrap()
            .parse()
            .unwrap();
        let rows: Vec<String> = lines.map(str::to_string).collect();
        Vector { text, size, rows }
    }

    fn rows(m: &QrMatrix) -> Vec<String> {
        m.modules
            .chunks(m.size)
            .map(|r| r.iter().map(|&b| if b == 1 { '#' } else { '.' }).collect())
            .collect()
    }

    fn decode(m: &QrMatrix) -> String {
        // Scale 4x with a 4-module quiet zone, as a renderer would draw it.
        let (scale, quiet) = (4, 4);
        let side = (m.size + 2 * quiet) * scale;
        let mut img = rqrr::PreparedImage::prepare_from_greyscale(side, side, |x, y| {
            let (mx, my) = (x / scale, y / scale);
            let dark = mx >= quiet
                && my >= quiet
                && mx < quiet + m.size
                && my < quiet + m.size
                && m.modules[(my - quiet) * m.size + (mx - quiet)] == 1;
            if dark { 0 } else { 255 }
        });
        let grids = img.detect_grids();
        assert_eq!(grids.len(), 1, "exactly one QR code in the image");
        grids[0].decode().unwrap().1
    }

    #[test]
    fn qr_matrix_matches_pinned_vector() {
        let v = parse_vector();
        let m = qr_matrix(&v.text).unwrap();
        assert_eq!(m.size, v.size);
        assert_eq!(v.rows.len(), v.size);
        assert_eq!(rows(&m), v.rows);
    }

    #[test]
    fn qr_matrix_decodes_to_its_text() {
        let v = parse_vector();
        let long = format!("vault-pair:v1:{}", "AbC0123456789-_".repeat(20));
        for text in [v.text.as_str(), "x", long.as_str()] {
            let m = qr_matrix(text).unwrap();
            assert_eq!(m.modules.len(), m.size * m.size);
            assert_eq!((m.size - 17) % 4, 0, "size is 17 + 4 * version");
            assert!(m.modules.iter().all(|&b| b <= 1));
            assert_eq!(decode(&m), text);
        }
    }

    #[test]
    fn qr_matrix_has_finder_patterns() {
        let m = qr_matrix("finder").unwrap();
        let at = |x: usize, y: usize| m.modules[y * m.size + x];
        for (ox, oy) in [(0, 0), (m.size - 7, 0), (0, m.size - 7)] {
            // Outer ring dark, next ring light, 3x3 core dark.
            for i in 0..7 {
                assert_eq!(at(ox + i, oy), 1);
                assert_eq!(at(ox + i, oy + 6), 1);
                assert_eq!(at(ox, oy + i), 1);
                assert_eq!(at(ox + 6, oy + i), 1);
            }
            for i in 1..6 {
                assert_eq!(at(ox + i, oy + 1), 0);
                assert_eq!(at(ox + 1, oy + i), 0);
            }
            for y in 2..5 {
                for x in 2..5 {
                    assert_eq!(at(ox + x, oy + y), 1);
                }
            }
        }
    }

    #[test]
    fn qr_matrix_refuses_oversized_text() {
        let e = qr_matrix(&"x".repeat(5000)).unwrap_err();
        assert!(e.contains("cannot encode"), "{e}");
    }

    #[test]
    fn state_names() {
        assert_eq!(state_name(&SessionState::Pairing), "pairing");
        assert_eq!(
            state_name(&SessionState::AwaitingApproval),
            "awaitingApproval"
        );
        assert_eq!(state_name(&SessionState::Active), "active");
        assert_eq!(
            state_name(&SessionState::Closed(CloseReason::Locked)),
            "closed"
        );
    }

    #[test]
    fn reason_names_are_camel_case_variant_names() {
        let all = [
            (CloseReason::Denied, "denied"),
            (CloseReason::Locked, "locked"),
            (CloseReason::Terminated("x".into()), "terminated"),
            (CloseReason::Disconnected, "disconnected"),
            (CloseReason::Cancelled, "cancelled"),
            (CloseReason::CommitFailed, "commitFailed"),
            (CloseReason::SendFailed, "sendFailed"),
            (CloseReason::ProtocolError, "protocolError"),
            (CloseReason::Closed, "closed"),
            (
                CloseReason::RelayUnreachable("x".into()),
                "relayUnreachable",
            ),
            (CloseReason::PairingFailed("x".into()), "pairingFailed"),
            (CloseReason::KeepaliveFailed("x".into()), "keepaliveFailed"),
            (CloseReason::KeepaliveTimedOut, "keepaliveTimedOut"),
            (CloseReason::Failed("x".into()), "failed"),
            (CloseReason::WrongApp, "wrongApp"),
        ];
        for (r, name) in all {
            assert_eq!(reason_name(&r), name);
            // camelCase of the Debug variant name.
            let debug = format!("{r:?}");
            let variant = debug.split('(').next().unwrap();
            let mut camel = variant[..1].to_lowercase();
            camel.push_str(&variant[1..]);
            assert_eq!(camel, name);
        }
    }

    #[test]
    fn origin_in_browser_is_location() {
        assert_eq!(
            resolve_origin(Some("http://LOCALHOST:8080"), None).unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            resolve_origin(Some("https://app.example:443"), Some("https://app.example")).unwrap(),
            "https://app.example"
        );
    }

    #[test]
    fn test_origin_cannot_override_location() {
        let e =
            resolve_origin(Some("https://app.example"), Some("https://other.example")).unwrap_err();
        assert!(e.contains("only for runtimes without location"), "{e}");
    }

    #[test]
    fn test_origin_outside_browser() {
        assert_eq!(
            resolve_origin(None, Some("http://localhost:8080")).unwrap(),
            "http://localhost:8080"
        );
        let e = resolve_origin(None, None).unwrap_err();
        assert!(e.contains("pass testOrigin"), "{e}");
        assert!(
            resolve_origin(None, Some("http://example.com")).is_err(),
            "http only for loopback"
        );
        assert!(resolve_origin(None, Some("https://a.example/path")).is_err());
    }
}
