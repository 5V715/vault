// SPDX-License-Identifier: MIT

//! Minimal AWS Signature Version 4 signer for S3 (header-based, payload always hashed).

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// Everything that goes into one signature besides the credentials.
pub(crate) struct SigningInput<'a> {
    pub method: &'a str,
    /// Already percent-encoded absolute path (S3 does not double-encode).
    pub canonical_uri: &'a str,
    /// Already encoded and sorted `k=v&k=v` query string (may be empty).
    pub canonical_query: &'a str,
    /// Headers to sign as `(lowercase name, value)`; must include `host`.
    pub headers: &'a [(&'a str, &'a str)],
    /// Lowercase hex SHA-256 of the body (the `x-amz-content-sha256` value).
    pub payload_sha256: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(b: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(b)
}

fn hmac(key: &[u8], msg: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(msg);
    Zeroizing::new(m.finalize().into_bytes().into())
}

/// Percent-encodes per SigV4 (unreserved characters kept; `/` kept iff `keep_slash`).
pub(crate) fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn normalize_value(v: &str) -> String {
    v.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn scope(date: &str, region: &str, service: &str) -> String {
    format!("{date}/{region}/{service}/aws4_request")
}

/// Returns the `Authorization` header value.
pub(crate) fn authorization(access_key: &str, secret_key: &str, inp: &SigningInput<'_>) -> String {
    let mut headers: Vec<(String, String)> = inp
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), normalize_value(v)))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        inp.method,
        inp.canonical_uri,
        inp.canonical_query,
        canonical_headers,
        signed_headers,
        inp.payload_sha256
    );
    let date = &inp.amz_date[..8];
    let scope = scope(date, inp.region, inp.service);
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{scope}\n{}",
        inp.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );

    let mut seed = Zeroizing::new(format!("AWS4{secret_key}"));
    let k_date = hmac(seed.as_bytes(), date.as_bytes());
    seed.clear();
    let k_region = hmac(&*k_date, inp.region.as_bytes());
    let k_service = hmac(&*k_region, inp.service.as_bytes());
    let k_signing = hmac(&*k_service, b"aws4_request");
    let signature = hex(&*hmac(&*k_signing, string_to_sign.as_bytes()));

    format!(
        "{ALGORITHM} Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

/// Formats unix seconds as `YYYYMMDDTHHMMSSZ`.
pub(crate) fn amz_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    // Civil-from-days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    // Vectors transcribed from the AWS S3 SigV4 documentation ("Examples: Signature
    // Calculations in AWS Signature Version 4", header-based authentication).
    #[test]
    fn sigv4_matches_aws_vectors() {
        // GET Object with Range.
        let a = authorization(
            AK,
            SK,
            &SigningInput {
                method: "GET",
                canonical_uri: "/test.txt",
                canonical_query: "",
                headers: &[
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("range", "bytes=0-9"),
                    ("x-amz-content-sha256", EMPTY),
                    ("x-amz-date", "20130524T000000Z"),
                ],
                payload_sha256: EMPTY,
                amz_date: "20130524T000000Z",
                region: "us-east-1",
                service: "s3",
            },
        );
        assert_eq!(
            a,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );

        // PUT Object with body "Welcome to Amazon S3." and storage class.
        let body_hash = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(
            body_hash,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let a = authorization(
            AK,
            SK,
            &SigningInput {
                method: "PUT",
                canonical_uri: "/test%24file.text",
                canonical_query: "",
                headers: &[
                    ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("x-amz-content-sha256", &body_hash),
                    ("x-amz-date", "20130524T000000Z"),
                    ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                ],
                payload_sha256: &body_hash,
                amz_date: "20130524T000000Z",
                region: "us-east-1",
                service: "s3",
            },
        );
        assert!(a.ends_with(
            "Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        ));

        // GET Bucket (List Objects) with query parameters.
        let a = authorization(
            AK,
            SK,
            &SigningInput {
                method: "GET",
                canonical_uri: "/",
                canonical_query: "max-keys=2&prefix=J",
                headers: &[
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("x-amz-content-sha256", EMPTY),
                    ("x-amz-date", "20130524T000000Z"),
                ],
                payload_sha256: EMPTY,
                amz_date: "20130524T000000Z",
                region: "us-east-1",
                service: "s3",
            },
        );
        assert!(a.ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn amz_date_formats() {
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_709_164_799), "20240228T235959Z");
        assert_eq!(amz_date(1_709_251_200), "20240301T000000Z");
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
    }

    #[test]
    fn uri_encoding() {
        assert_eq!(uri_encode("a b/c$~", true), "a%20b/c%24~");
        assert_eq!(uri_encode("seg/ab", false), "seg%2Fab");
    }
}
