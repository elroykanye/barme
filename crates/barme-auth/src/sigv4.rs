//! AWS Signature Version 4 verification for the S3 door.
//!
//! The server recomputes the signature the client should have produced and
//! compares. Correctness is pinned by a unit test against AWS's own published
//! example (the "GET Object" case from the SigV4 docs).
//!
//! Not yet handled: streaming/chunked payload signing (aws-chunked) and
//! presigned query-string auth. Header-based signing covers boto3, the AWS SDKs
//! and the MinIO clients in their default single-request mode.

use crate::{AuthError, Credentials, Principal};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

type HmacSha256 = Hmac<Sha256>;

/// Everything from an incoming request the verifier needs. The door fills this
/// from the wire: `path` and `query` must be the raw, still-encoded forms, and
/// header names must be lowercased.
pub struct SignedRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
}

impl SignedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Authenticate a request. No Authorization header means Anonymous (the caller
/// then decides whether that's allowed). A present but invalid signature is an
/// error.
pub fn verify_sigv4(creds: &Credentials, req: &SignedRequest) -> Result<Principal, AuthError> {
    let Some(auth) = req.header("authorization") else {
        return Ok(Principal::Anonymous);
    };
    let parsed = ParsedAuth::parse(auth)?;

    let secret = creds
        .secret(&parsed.access_key)
        .ok_or(AuthError::UnknownKey)?;
    let amz_date = req
        .header("x-amz-date")
        .ok_or(AuthError::MissingHeader("x-amz-date"))?;
    let payload_hash = req
        .header("x-amz-content-sha256")
        .ok_or(AuthError::MissingHeader("x-amz-content-sha256"))?;

    let expected = sign(
        secret,
        &req.method,
        &req.path,
        &req.query,
        &req.headers,
        &parsed.signed_headers,
        amz_date,
        &parsed.scope,
        payload_hash,
    )?;

    if constant_time_eq(expected.as_bytes(), parsed.signature.as_bytes()) {
        Ok(Principal::Owner(parsed.access_key))
    } else {
        Err(AuthError::SignatureMismatch)
    }
}

struct ParsedAuth {
    access_key: String,
    scope: String,
    signed_headers: Vec<String>,
    signature: String,
}

impl ParsedAuth {
    fn parse(header: &str) -> Result<Self, AuthError> {
        let rest = header
            .strip_prefix("AWS4-HMAC-SHA256 ")
            .ok_or(AuthError::MalformedHeader)?;

        let (mut credential, mut signed_headers, mut signature) = (None, None, None);
        for part in rest.split(',') {
            let (k, v) = part.trim().split_once('=').ok_or(AuthError::MalformedHeader)?;
            match k {
                "Credential" => credential = Some(v),
                "SignedHeaders" => signed_headers = Some(v),
                "Signature" => signature = Some(v),
                _ => {}
            }
        }

        let credential = credential.ok_or(AuthError::MalformedHeader)?;
        let signed_headers = signed_headers.ok_or(AuthError::MalformedHeader)?;
        let signature = signature.ok_or(AuthError::MalformedHeader)?;

        // Credential = <access_key>/<date>/<region>/<service>/aws4_request
        let (access_key, scope) = credential.split_once('/').ok_or(AuthError::MalformedHeader)?;

        Ok(ParsedAuth {
            access_key: access_key.to_string(),
            scope: scope.to_string(),
            signed_headers: signed_headers.split(';').map(str::to_string).collect(),
            signature: signature.to_string(),
        })
    }
}

/// Recompute the hex signature for a request. Region and service are read from
/// the credential scope so we sign exactly what the client scoped to.
#[allow(clippy::too_many_arguments)]
fn sign(
    secret: &str,
    method: &str,
    path: &str,
    query: &str,
    headers: &HashMap<String, String>,
    signed_headers: &[String],
    amz_date: &str,
    scope: &str,
    payload_hash: &str,
) -> Result<String, AuthError> {
    // scope = <date>/<region>/<service>/aws4_request
    let mut scope_parts = scope.split('/');
    let date_stamp = scope_parts.next().ok_or(AuthError::MalformedHeader)?;
    let region = scope_parts.next().ok_or(AuthError::MalformedHeader)?;
    let service = scope_parts.next().ok_or(AuthError::MalformedHeader)?;

    let mut canonical_headers = String::new();
    for name in signed_headers {
        let value = headers
            .get(name)
            .ok_or(AuthError::MalformedHeader)?
            .trim();
        canonical_headers.push_str(&format!("{name}:{value}\n"));
    }
    let signed = signed_headers.join(";");

    let canonical_request = format!(
        "{method}\n{path}\n{}\n{canonical_headers}\n{signed}\n{payload_hash}",
        canonical_query(query)
    );

    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    Ok(hex::encode(hmac(&k_signing, string_to_sign.as_bytes())))
}

fn canonical_query(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = raw
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let mut it = kv.splitn(2, '=');
            let k = percent_decode(it.next().unwrap_or(""));
            let v = percent_decode(it.next().unwrap_or(""));
            (uri_encode(&k, true), uri_encode(&v, true))
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCESS: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const EMPTY_SHA: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    // AWS SigV4 test suite, "get-vanilla": the canonical reference case.
    const VANILLA_SIG: &str =
        "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31";

    #[test]
    fn matches_aws_reference_signature() {
        let headers = HashMap::from([
            ("host".into(), "example.amazonaws.com".into()),
            ("x-amz-date".into(), "20150830T123600Z".into()),
        ]);
        let sig = sign(
            SECRET,
            "GET",
            "/",
            "",
            &headers,
            &["host".into(), "x-amz-date".into()],
            "20150830T123600Z",
            "20150830/us-east-1/service/aws4_request",
            EMPTY_SHA,
        )
        .unwrap();
        assert_eq!(sig, VANILLA_SIG);
    }

    fn creds() -> Credentials {
        Credentials {
            keys: HashMap::from([(ACCESS.to_string(), SECRET.to_string())]),
        }
    }

    /// Build a real S3-style signed request using our own (now trusted) signer,
    /// so the verify path exercises parsing, header extraction, and comparison.
    fn signed_request() -> SignedRequest {
        let scope = "20150830/us-east-1/s3/aws4_request";
        let signed = ["host", "x-amz-content-sha256", "x-amz-date"];
        let mut headers = HashMap::from([
            ("host".into(), "barme.local".into()),
            ("x-amz-content-sha256".into(), EMPTY_SHA.into()),
            ("x-amz-date".into(), "20150830T123600Z".into()),
        ]);

        let sig = sign(
            SECRET,
            "GET",
            "/mybucket/key.txt",
            "",
            &headers,
            &signed.map(String::from),
            "20150830T123600Z",
            scope,
            EMPTY_SHA,
        )
        .unwrap();

        headers.insert(
            "authorization".into(),
            format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS}/{scope}, \
                 SignedHeaders={}, Signature={sig}",
                signed.join(";")
            ),
        );
        SignedRequest {
            method: "GET".into(),
            path: "/mybucket/key.txt".into(),
            query: String::new(),
            headers,
        }
    }

    #[test]
    fn verifies_a_correctly_signed_request() {
        assert_eq!(
            verify_sigv4(&creds(), &signed_request()).unwrap(),
            Principal::Owner(ACCESS.into())
        );
    }

    #[test]
    fn rejects_a_bad_signature() {
        let mut req = signed_request();
        // Replace the real signature with zeros.
        let auth = req.headers.get("authorization").unwrap();
        let cut = auth.rfind("Signature=").unwrap() + "Signature=".len();
        let tampered = format!("{}{}", &auth[..cut], "0".repeat(64));
        req.headers.insert("authorization".into(), tampered);
        assert!(matches!(
            verify_sigv4(&creds(), &req),
            Err(AuthError::SignatureMismatch)
        ));
    }

    #[test]
    fn no_authorization_is_anonymous() {
        let mut req = signed_request();
        req.headers.remove("authorization");
        assert_eq!(verify_sigv4(&creds(), &req).unwrap(), Principal::Anonymous);
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(matches!(
            verify_sigv4(&Credentials::default(), &signed_request()),
            Err(AuthError::UnknownKey)
        ));
    }
}
