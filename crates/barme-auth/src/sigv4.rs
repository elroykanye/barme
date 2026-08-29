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
use std::time::{SystemTime, UNIX_EPOCH};
use time::PrimitiveDateTime;

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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuthError::MalformedHeader)?
        .as_secs() as i64;
    verify_sigv4_at(creds, req, now)
}

fn verify_sigv4_at(
    creds: &Credentials,
    req: &SignedRequest,
    now_unix: i64,
) -> Result<Principal, AuthError> {
    let (parsed, query, amz_date, payload_hash) = if let Some(auth) = req.header("authorization") {
        let parsed = ParsedAuth::parse(auth)?;
        let amz_date = req
            .header("x-amz-date")
            .ok_or(AuthError::MissingHeader("x-amz-date"))?;
        let payload_hash = req
            .header("x-amz-content-sha256")
            .ok_or(AuthError::MissingHeader("x-amz-content-sha256"))?;
        (parsed, req.query.clone(), amz_date.to_string(), payload_hash.to_string())
    } else if query_param(&req.query, "X-Amz-Algorithm").is_some() {
        let parsed = ParsedAuth::parse_query(&req.query)?;
        let amz_date = required_query_param(&req.query, "X-Amz-Date")?;
        ensure_presign_not_expired(&req.query, &amz_date, now_unix)?;
        (
            parsed,
            query_without_signature(&req.query),
            amz_date,
            "UNSIGNED-PAYLOAD".into(),
        )
    } else {
        return Ok(Principal::Anonymous);
    };

    let secret = creds
        .secret(&parsed.access_key)
        .ok_or(AuthError::UnknownKey)?;

    let expected = sign(
        secret,
        &req.method,
        &req.path,
        &query,
        &req.headers,
        &parsed.signed_headers,
        &amz_date,
        &parsed.scope,
        &payload_hash,
    )?;

    if constant_time_eq(expected.as_bytes(), parsed.signature.as_bytes()) {
        Ok(Principal::Owner(parsed.access_key))
    } else {
        Err(AuthError::SignatureMismatch)
    }
}

fn ensure_presign_not_expired(
    query: &str,
    amz_date: &str,
    now_unix: i64,
) -> Result<(), AuthError> {
    let expires: i64 = required_query_param(query, "X-Amz-Expires")?
        .parse()
        .map_err(|_| AuthError::MalformedHeader)?;
    if !(0..=604_800).contains(&expires) {
        return Err(AuthError::MalformedHeader);
    }
    let format = time::format_description::parse_borrowed::<2>(
        "[year][month][day]T[hour][minute][second]Z",
    )
    .map_err(|_| AuthError::MalformedHeader)?;
    let issued = PrimitiveDateTime::parse(amz_date, &format)
        .map_err(|_| AuthError::MalformedHeader)?
        .assume_utc()
        .unix_timestamp();
    if issued > now_unix.saturating_add(900)
        || now_unix > issued.saturating_add(expires)
    {
        return Err(AuthError::SignatureMismatch);
    }
    Ok(())
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

    fn parse_query(query: &str) -> Result<Self, AuthError> {
        if required_query_param(query, "X-Amz-Algorithm")?.as_str() != "AWS4-HMAC-SHA256" {
            return Err(AuthError::MalformedHeader);
        }
        let credential = required_query_param(query, "X-Amz-Credential")?;
        let signed_headers = required_query_param(query, "X-Amz-SignedHeaders")?;
        let signature = required_query_param(query, "X-Amz-Signature")?;
        let (access_key, scope) = credential
            .split_once('/')
            .ok_or(AuthError::MalformedHeader)?;

        Ok(ParsedAuth {
            access_key: access_key.to_string(),
            scope: scope.to_string(),
            signed_headers: signed_headers.split(';').map(str::to_string).collect(),
            signature,
        })
    }
}

fn query_param(raw: &str, wanted: &str) -> Option<String> {
    raw.split('&').filter(|part| !part.is_empty()).find_map(|part| {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        (percent_decode(key) == wanted).then(|| percent_decode(value))
    })
}

fn required_query_param(raw: &str, wanted: &str) -> Result<String, AuthError> {
    let mut matches = raw.split('&').filter(|part| !part.is_empty()).filter_map(|part| {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        (percent_decode(key) == wanted).then(|| percent_decode(value))
    });
    let value = matches.next().ok_or(AuthError::MalformedHeader)?;
    if matches.next().is_some() {
        return Err(AuthError::SignatureMismatch);
    }
    Ok(value)
}

fn query_without_signature(raw: &str) -> String {
    raw.split('&')
        .filter(|part| {
            let key = part.split_once('=').map_or(*part, |(key, _)| key);
            percent_decode(key) != "X-Amz-Signature"
        })
        .collect::<Vec<_>>()
        .join("&")
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
        Credentials::single(ACCESS, SECRET)
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

    fn botocore_presigned_get() -> SignedRequest {
        // Generated by boto3 1.43.72's public S3 presigning API at
        // 2026-08-17T12:00:00Z. Keeping the signer outside this crate prevents
        // the verifier and fixture from agreeing on the same mistake.
        let query = concat!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&",
            "X-Amz-Credential=AKIDEXAMPLE%2F20260817%2Fus-east-1%2Fs3%2Faws4_request&",
            "X-Amz-Date=20260817T120000Z&",
            "X-Amz-Expires=900&",
            "X-Amz-SignedHeaders=host&",
            "X-Amz-Signature=66b10d4b3e938da23092182bff453f832540bd89f9e6e64324ca5927167faba2",
        );
        SignedRequest {
            method: "GET".into(),
            path: "/photos/cat.txt".into(),
            query: query.into(),
            headers: HashMap::from([("host".into(), "barme.local".into())]),
        }
    }

    #[test]
    fn verifies_a_botocore_presigned_get() {
        assert_eq!(
            verify_sigv4_at(&creds(), &botocore_presigned_get(), 1_786_968_840).unwrap(),
            Principal::Owner(ACCESS.into())
        );
    }

    #[test]
    fn rejects_tampered_presigned_path_header_query_and_signature() {
        let mut path = botocore_presigned_get();
        path.path = "/photos/dog.txt".into();

        let mut header = botocore_presigned_get();
        header.headers.insert("host".into(), "elsewhere.local".into());

        let mut query = botocore_presigned_get();
        query.query = query.query.replace("X-Amz-Expires=900", "X-Amz-Expires=901");

        let mut signature = botocore_presigned_get();
        let cut = signature.query.rfind("X-Amz-Signature=").unwrap()
            + "X-Amz-Signature=".len();
        signature.query.replace_range(cut.., &"0".repeat(64));

        let mut duplicate_signature = botocore_presigned_get();
        duplicate_signature
            .query
            .push_str(&format!("&X-Amz-Signature={}", "0".repeat(64)));

        for request in [&path, &header, &query, &signature, &duplicate_signature] {
            assert!(matches!(
                verify_sigv4_at(&creds(), request, 1_786_968_840),
                Err(AuthError::SignatureMismatch)
            ));
        }
    }

    #[test]
    fn preserves_a_literal_plus_in_a_botocore_presigned_query() {
        // Generated by boto3 with ResponseContentDisposition containing `a+b`.
        // RFC 3986 permits the signed `%2B` byte to travel as a literal `+`;
        // unlike form encoding, SigV4 must not reinterpret it as a space.
        let encoded_query = concat!(
            "response-content-disposition=attachment%3B%20filename%3Da%2Bb.txt&",
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&",
            "X-Amz-Credential=AKIDEXAMPLE%2F20260817%2Fus-east-1%2Fs3%2Faws4_request&",
            "X-Amz-Date=20260817T120000Z&",
            "X-Amz-Expires=900&",
            "X-Amz-SignedHeaders=host&",
            "X-Amz-Signature=b91b6d48d4cccd61871dc9f1ce117a70af27fc7880775a794d04e3f150b1380b",
        );
        let request = |query: String| SignedRequest {
            method: "GET".into(),
            path: "/photos/plus.txt".into(),
            query,
            headers: HashMap::from([("host".into(), "barme.local".into())]),
        };

        for query in [encoded_query.to_string(), encoded_query.replace("%2B", "+")] {
            assert_eq!(
                verify_sigv4_at(&creds(), &request(query), 1_786_968_840).unwrap(),
                Principal::Owner(ACCESS.into())
            );
        }
        assert!(matches!(
            verify_sigv4_at(
                &creds(),
                &request(encoded_query.replace("%2B", "%20")),
                1_786_968_840,
            ),
            Err(AuthError::SignatureMismatch)
        ));
    }

    #[test]
    fn rejects_an_expired_botocore_presign() {
        // A valid boto3 URL issued at 2020-01-02T03:04:05Z for 60 seconds.
        let query = concat!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&",
            "X-Amz-Credential=AKIDEXAMPLE%2F20200102%2Fus-east-1%2Fs3%2Faws4_request&",
            "X-Amz-Date=20200102T030405Z&",
            "X-Amz-Expires=60&",
            "X-Amz-SignedHeaders=host&",
            "X-Amz-Signature=4175aa5da874e0160a416b283466fc94945bab90dfdd2630b2af343f8c52da32",
        );
        let req = SignedRequest {
            method: "GET".into(),
            path: "/photos/expired.txt".into(),
            query: query.into(),
            headers: HashMap::from([("host".into(), "barme.local".into())]),
        };

        assert!(verify_sigv4_at(&creds(), &req, 1_577_934_306).is_err());
    }

    #[test]
    fn rejects_a_presign_more_than_fifteen_minutes_in_the_future() {
        let req = botocore_presigned_get();
        assert!(verify_sigv4_at(&creds(), &req, 1_786_967_099).is_err());
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
