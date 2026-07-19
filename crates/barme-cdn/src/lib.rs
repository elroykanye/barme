//! Public delivery door: a CDN-friendly front for serving object bytes.
//!
//! Two URL styles, both anonymous:
//!   /cdn/{hash}            immutable. A content hash never changes, so this is
//!                          served `Cache-Control: immutable, max-age=1yr` and
//!                          caches forever at every layer. Capability URL: the
//!                          256-bit hash is the token.
//!   /public/{bucket}/{key} friendly. Only served when the bucket is public;
//!                          revalidated with an ETag (the object id) so a stale
//!                          cache gets a cheap 304.
//!
//! Both support conditional GET (If-None-Match -> 304) and HTTP range requests
//! (206 Partial Content) for media streaming and resumable downloads.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use barme_core::Hash;
use barme_engine::Engine;
use serde::Deserialize;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

pub fn app(engine: Arc<Engine>, cors_origins: &[String]) -> Router {
    Router::new()
        .route("/cdn/{hash}", get(by_hash))
        .route("/public/{bucket}/{*key}", get(by_key))
        .route("/s/{bucket}/{*key}", get(by_presign))
        .layer(cors_layer(cors_origins))
        .layer(TraceLayer::new_for_http())
        .with_state(engine)
}

/// CORS for the delivery door. `["*"]` (the default) stays permissive; a specific
/// list restricts `Access-Control-Allow-Origin` to those origins. An entry that
/// isn't a valid origin is dropped; an empty result allows no cross-origin call.
fn cors_layer(origins: &[String]) -> CorsLayer {
    use tower_http::cors::Any;
    if origins.iter().any(|o| o == "*") {
        CorsLayer::permissive()
    } else {
        let list: Vec<HeaderValue> = origins.iter().filter_map(|o| o.parse().ok()).collect();
        CorsLayer::new()
            .allow_origin(list)
            .allow_methods(Any)
            .allow_headers(Any)
    }
}

pub async fn serve(
    engine: Arc<Engine>,
    cors_origins: Vec<String>,
    listener: tokio::net::TcpListener,
) -> std::io::Result<()> {
    axum::serve(listener, app(engine, &cors_origins)).await
}

/// Immutable delivery by content hash. Anyone holding the hash may fetch it.
async fn by_hash(
    State(engine): State<Arc<Engine>>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(id) = hash.parse::<Hash>() else {
        return (StatusCode::BAD_REQUEST, "malformed hash").into_response();
    };
    let manifest = match engine.object_manifest(&id) {
        Ok(Some(m)) => m,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let bytes = match engine.read_object(&id) {
        Ok(b) => b,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    deliver(bytes, &manifest.original.content_type, &quoted(&id.to_string()), true, &headers)
}

/// Friendly delivery by bucket/key, only for public buckets.
async fn by_key(
    State(engine): State<Arc<Engine>>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    // Non-public buckets are indistinguishable from missing ones here.
    if !engine.is_public(&bucket).unwrap_or(false) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let manifest = match engine.manifest(&bucket, &key) {
        Ok(Some(m)) => m,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let etag = quoted(&manifest.object_id.to_string());

    // Skip reading the bytes if the client already has this version.
    if if_none_match(&headers, &etag) {
        return not_modified(&etag, false);
    }
    let bytes = match engine.get(&bucket, &key) {
        Ok(Some(b)) => b,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    deliver(bytes, &manifest.original.content_type, &etag, false, &headers)
}

/// Time-limited share delivery. A valid, unexpired presigned signature serves
/// the object's bytes even from a private pot. The signing secret is the same
/// one the native door signs links with (the first owner key's secret).
#[derive(Deserialize)]
struct Presigned {
    exp: u64,
    sig: String,
}

async fn by_presign(
    State(engine): State<Arc<Engine>>,
    Path((bucket, key)): Path<(String, String)>,
    Query(q): Query<Presigned>,
    headers: HeaderMap,
) -> Response {
    let Some(secret) = engine.signing_secret() else {
        // Open mode (no owner key): nothing to verify against, so no shares.
        return StatusCode::NOT_FOUND.into_response();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if !barme_auth::verify_presign(&secret, &bucket, &key, q.exp, &q.sig, now) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let manifest = match engine.manifest(&bucket, &key) {
        Ok(Some(m)) => m,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let etag = quoted(&manifest.object_id.to_string());
    if if_none_match(&headers, &etag) {
        return not_modified(&etag, false);
    }
    let bytes = match engine.get(&bucket, &key) {
        Ok(Some(b)) => b,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    deliver(bytes, &manifest.original.content_type, &etag, false, &headers)
}

fn deliver(
    bytes: Vec<u8>,
    content_type: &str,
    etag: &str,
    immutable: bool,
    req: &HeaderMap,
) -> Response {
    if if_none_match(req, etag) {
        return not_modified(etag, immutable);
    }

    let total = bytes.len();
    if let Some((start, end)) = req
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_range(v, total))
    {
        let slice = bytes[start..=end].to_vec();
        let mut h = base_headers(content_type, etag, immutable);
        h.insert(
            header::CONTENT_RANGE,
            hv(&format!("bytes {start}-{end}/{total}")),
        );
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(slice.len()));
        return (StatusCode::PARTIAL_CONTENT, h, slice).into_response();
    }

    let mut h = base_headers(content_type, etag, immutable);
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(total));
    (StatusCode::OK, h, bytes).into_response()
}

fn base_headers(content_type: &str, etag: &str, immutable: bool) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, hv(content_type));
    h.insert(header::ETAG, hv(etag));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CACHE_CONTROL, hv(cache_control(immutable)));
    h
}

fn not_modified(etag: &str, immutable: bool) -> Response {
    let mut h = HeaderMap::new();
    h.insert(header::ETAG, hv(etag));
    h.insert(header::CACHE_CONTROL, hv(cache_control(immutable)));
    (StatusCode::NOT_MODIFIED, h).into_response()
}

fn cache_control(immutable: bool) -> &'static str {
    if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=60, must-revalidate"
    }
}

fn if_none_match(req: &HeaderMap, etag: &str) -> bool {
    req.get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "*" || v.split(',').any(|t| t.trim() == etag))
        .unwrap_or(false)
}

/// Parse a single `bytes=start-end` range against a known total. Supports open
/// ends (`start-`) and suffix ranges (`-N`). Returns an inclusive [start, end].
fn parse_range(value: &str, total: usize) -> Option<(usize, usize)> {
    if total == 0 {
        return None;
    }
    let spec = value.strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    let (start, end) = if s.is_empty() {
        let n: usize = e.parse().ok()?;
        let n = n.min(total);
        (total - n, total - 1)
    } else {
        let start: usize = s.parse().ok()?;
        let end = if e.is_empty() {
            total - 1
        } else {
            e.parse::<usize>().ok()?.min(total - 1)
        };
        (start, end)
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end))
}

fn quoted(s: &str) -> String {
    format!("\"{s}\"")
}

fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or(HeaderValue::from_static(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=10-", 100), Some((10, 99)));
        assert_eq!(parse_range("bytes=-20", 100), Some((80, 99)));
        assert_eq!(parse_range("bytes=200-300", 100), None); // out of range
        assert_eq!(parse_range("bytes=5-3", 100), None); // inverted
        assert_eq!(parse_range("garbage", 100), None);
        assert_eq!(parse_range("bytes=0-9", 0), None); // empty object
    }

    #[test]
    fn if_none_match_matches() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"abc\""));
        assert!(if_none_match(&h, "\"abc\""));
        assert!(!if_none_match(&h, "\"xyz\""));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(if_none_match(&h, "\"anything\""));
    }

    // --- presigned share links, end to end through the delivery door ---

    use axum::body::Body;
    use axum::http::Request;
    use barme_core::KeyRecord;
    use barme_engine::{Engine, Policy};
    use tower::ServiceExt;

    const FUTURE: u64 = 4_102_444_800; // year 2100, comfortably unexpired
    const PAST: u64 = 1; // 1970, long expired

    /// An engine with one owner key (so a signing secret exists) and a private
    /// object. Returns the signing secret the door will verify against.
    fn engine_with_private_object() -> (tempfile::TempDir, Arc<Engine>, String) {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path(), Policy::default()).unwrap();
        engine
            .create_key(&KeyRecord {
                access_key: "owner".into(),
                secret_key: "shh-signing-secret".into(),
                read_only: false,
                pots: vec![],
                created_at: String::new(),
            })
            .unwrap();
        // Private pot (no public_read): only a valid presign should serve it.
        engine
            .put("private", "doc.txt", b"time-limited bytes", "text/plain")
            .unwrap();
        let secret = engine.signing_secret().unwrap();
        (dir, Arc::new(engine), secret)
    }

    async fn share_status(engine: Arc<Engine>, uri: &str) -> StatusCode {
        app(engine, &["*".to_string()])
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn valid_presign_serves_a_private_object() {
        let (_d, engine, secret) = engine_with_private_object();
        let sig = barme_auth::presign(&secret, "private", "doc.txt", FUTURE);
        let uri = format!("/s/private/doc.txt?exp={FUTURE}&sig={sig}");
        assert_eq!(share_status(engine, &uri).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn expired_presign_is_forbidden() {
        let (_d, engine, secret) = engine_with_private_object();
        // Correctly signed, but for an expiry in the past.
        let sig = barme_auth::presign(&secret, "private", "doc.txt", PAST);
        let uri = format!("/s/private/doc.txt?exp={PAST}&sig={sig}");
        assert_eq!(share_status(engine, &uri).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn tampered_signature_is_forbidden() {
        let (_d, engine, _secret) = engine_with_private_object();
        let uri = format!("/s/private/doc.txt?exp={FUTURE}&sig={}", "0".repeat(64));
        assert_eq!(share_status(engine, &uri).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_signature_does_not_transfer_to_another_key() {
        // The signature binds the path, so a link minted for one object must not
        // unlock a different one under the same expiry.
        let (_d, engine, secret) = engine_with_private_object();
        engine.put("private", "other.txt", b"not shared", "text/plain").unwrap();
        let sig = barme_auth::presign(&secret, "private", "doc.txt", FUTURE); // for doc.txt
        let uri = format!("/s/private/other.txt?exp={FUTURE}&sig={sig}"); // used on other.txt
        assert_eq!(share_status(engine, &uri).await, StatusCode::FORBIDDEN);
    }
}
