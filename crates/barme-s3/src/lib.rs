//! S3-compatible front door.
//!
//! bucket/key/object maps almost directly onto bucket/pointer/manifest:
//!   PUT    -> engine write path (single, or one part of a multipart upload)
//!   GET    -> engine read path (or ListParts when `?uploadId` is present)
//!   DELETE -> move/clear pointer (or AbortMultipartUpload with `?uploadId`)
//!   HEAD   -> manifest lookup; etag is the content hash
//!   POST   -> multipart lifecycle: `?uploads` creates, `?uploadId` completes
//!   List   -> list pointers
//!
//! Multipart is dispatched by query parameters on the same object path, the way
//! S3 does it. The long tail of bucket sub-resources (ACLs, lifecycle, policies)
//! comes later.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, RawQuery, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Router,
};
use barme_auth::{authorize, verify_sigv4, Action, Credentials, Principal, SignedRequest};
use barme_engine::{Engine, EngineError, PartMeta};
use futures_util::{StreamExt, TryStreamExt};
use tokio_util::io::{StreamReader, SyncIoBridge};

/// S3 clients expect a Content-Type on every write; use this when they omit one.
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Cap on the CompleteMultipartUpload request body. It only carries a part list;
/// 10k parts at ~120 bytes each is ~1.2 MiB, so 16 MiB is comfortable headroom.
const MAX_COMPLETE_BODY: usize = 16 * 1024 * 1024;

/// Anything the engine hands back becomes a status + message. Not-found is
/// modelled as an `Option` on the read paths, so it never reaches here.
enum S3Error {
    Engine(EngineError),
    /// The upload's blocking task failed to run (panic or cancellation).
    Internal(String),
    /// Malformed client input the engine never saw (a bad query parameter).
    BadRequest(String),
}

impl From<EngineError> for S3Error {
    fn from(e: EngineError) -> Self {
        S3Error::Engine(e)
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            S3Error::Engine(e @ EngineError::InvalidKey(..)) => {
                (StatusCode::BAD_REQUEST, e.to_string())
            }
            S3Error::Engine(e @ EngineError::TooLarge { .. }) => {
                (StatusCode::PAYLOAD_TOO_LARGE, e.to_string())
            }
            S3Error::Engine(e @ EngineError::Upload(..)) => {
                (StatusCode::BAD_REQUEST, e.to_string())
            }
            S3Error::Engine(e @ EngineError::NoSuchUpload(..)) => {
                (StatusCode::NOT_FOUND, e.to_string())
            }
            S3Error::Engine(e) if e.is_bad_input() => (StatusCode::BAD_REQUEST, e.to_string()),
            S3Error::Engine(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            S3Error::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            S3Error::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
        };
        (status, msg).into_response()
    }
}

/// Shared state. Keys are read live from the engine's key store per request; an
/// empty store means the door runs open (no auth), convenient for local dev.
#[derive(Clone)]
pub struct S3State {
    pub engine: Arc<Engine>,
    /// Largest accepted upload body, in bytes. Enforced by the router.
    pub max_upload_bytes: usize,
}

/// The router, decoupled from any port so tests can drive it directly.
pub fn app(state: S3State) -> Router {
    let max_upload = state.max_upload_bytes;
    Router::new()
        // Pot-level (S3 bucket) operations.
        .route("/", get(list_buckets))
        .route("/{bucket}", put(create_bucket))
        .route("/{bucket}", axum::routing::head(head_bucket))
        .route("/{bucket}", delete(delete_bucket))
        // Object-level operations (and the multipart sequence by query param).
        .route("/{bucket}/{*key}", put(put_object))
        .route("/{bucket}/{*key}", get(get_object))
        .route("/{bucket}/{*key}", post(post_object))
        .route("/{bucket}/{*key}", delete(delete_object))
        // HEAD shares the GET route in axum; register it explicitly for clarity.
        .route("/{bucket}/{*key}", axum::routing::head(head_object))
        // Bound the buffered upload body; over the limit gets 413.
        .layer(axum::extract::DefaultBodyLimit::max(max_upload))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Serve on a pre-bound listener until the process ends.
pub async fn serve(state: S3State, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, app(state)).await
}

/// Verify the SigV4 signature, then authorize against the bucket's visibility.
/// With no credentials configured the request passes straight through.
async fn authenticate(State(st): State<S3State>, req: Request, next: Next) -> Response {
    let keys = st.engine.list_keys().unwrap_or_default();
    if keys.is_empty() {
        return next.run(req).await; // open mode: no keys configured
    }
    let creds = Credentials::from_records(keys);

    let mut headers = std::collections::HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers.insert(name.as_str().to_ascii_lowercase(), v.to_string());
        }
    }
    let signed = SignedRequest {
        method: req.method().as_str().to_string(),
        path: req.uri().path().to_string(),
        query: req.uri().query().unwrap_or("").to_string(),
        headers,
    };

    let principal = match verify_sigv4(&creds, &signed) {
        Ok(p) => p,
        Err(_) => return (StatusCode::FORBIDDEN, "invalid signature").into_response(),
    };

    let bucket = signed
        .path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    let action = match *req.method() {
        Method::GET | Method::HEAD => Action::Read,
        Method::DELETE => Action::Delete,
        _ => Action::Write,
    };
    let public = st.engine.is_public(bucket).unwrap_or(false);
    let record = match &principal {
        Principal::Owner(access) => creds.record(access),
        Principal::Anonymous => None,
    };

    if !authorize(record, action, bucket, public) {
        return (StatusCode::FORBIDDEN, "access denied").into_response();
    }
    next.run(req).await
}

/// PUT is either a whole-object write or one part of a multipart upload, told
/// apart by the `uploadId` + `partNumber` query parameters.
async fn put_object(
    State(st): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    let params = parse_query(query.as_deref());
    if let (Some(upload_id), Some(pn)) = (params.get("uploadId"), params.get("partNumber")) {
        return upload_part(&st, upload_id, pn, body).await;
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string();

    // Stream the body straight into the engine on a blocking task, so a large
    // object never fully buffers in memory. barme-auth verifies SigV4 from the
    // headers only (no payload-hash check), so the body doesn't need buffering
    // for the signature.
    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let sync_reader = SyncIoBridge::new(StreamReader::new(stream));
    let engine = st.engine.clone();
    let max = st.max_upload_bytes as u64;
    let object_id = tokio::task::spawn_blocking(move || {
        engine.put_stream(&bucket, &key, sync_reader, &content_type, max)
    })
    .await
    .map_err(|e| S3Error::Internal(e.to_string()))??;

    let mut out = HeaderMap::new();
    out.insert(header::ETAG, etag(&object_id.to_string()));
    Ok((StatusCode::OK, out).into_response())
}

/// Stream one part of a multipart upload into the store and return its ETag.
async fn upload_part(
    st: &S3State,
    upload_id: &str,
    part_number: &str,
    body: Body,
) -> Result<Response, S3Error> {
    let part_number: u32 = part_number
        .parse()
        .map_err(|_| S3Error::BadRequest("partNumber must be a positive integer".into()))?;

    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let sync_reader = SyncIoBridge::new(StreamReader::new(stream));
    let engine = st.engine.clone();
    let max = st.max_upload_bytes as u64;
    let uid = upload_id.to_string();
    let meta = tokio::task::spawn_blocking(move || {
        engine.upload_part(&uid, part_number, sync_reader, max)
    })
    .await
    .map_err(|e| S3Error::Internal(e.to_string()))??;

    let mut out = HeaderMap::new();
    out.insert(header::ETAG, etag(&meta.etag));
    Ok((StatusCode::OK, out).into_response())
}

/// POST drives the multipart lifecycle: `?uploads` creates an upload, `?uploadId`
/// completes one.
async fn post_object(
    State(st): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    let params = parse_query(query.as_deref());

    if params.contains_key("uploads") {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(DEFAULT_CONTENT_TYPE)
            .to_string();
        let engine = st.engine.clone();
        let (b, k) = (bucket.clone(), key.clone());
        let upload_id = tokio::task::spawn_blocking(move || {
            engine.create_multipart(&b, &k, &content_type)
        })
        .await
        .map_err(|e| S3Error::Internal(e.to_string()))??;
        return Ok(xml_response(initiate_xml(&bucket, &key, &upload_id)));
    }

    if let Some(upload_id) = params.get("uploadId") {
        // The body lists the parts the client wants stitched, in order.
        let bytes = axum::body::to_bytes(body, MAX_COMPLETE_BODY)
            .await
            .map_err(|e| S3Error::BadRequest(format!("reading complete body: {e}")))?;
        let order = parse_complete_parts(&bytes);
        let engine = st.engine.clone();
        let uid = upload_id.clone();
        let object_id = tokio::task::spawn_blocking(move || {
            engine.complete_multipart(&uid, &order)
        })
        .await
        .map_err(|e| S3Error::Internal(e.to_string()))??;
        return Ok(xml_response(complete_xml(&bucket, &key, &object_id.to_string())));
    }

    Ok(StatusCode::NOT_IMPLEMENTED.into_response())
}

/// GET is either an object read or, with `?uploadId`, a ListParts.
async fn get_object(
    State(st): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, S3Error> {
    let params = parse_query(query.as_deref());
    if let Some(upload_id) = params.get("uploadId") {
        let engine = st.engine.clone();
        let uid = upload_id.clone();
        let listed = tokio::task::spawn_blocking(move || engine.list_parts(&uid))
            .await
            .map_err(|e| S3Error::Internal(e.to_string()))??;
        return Ok(match listed {
            Some(l) => xml_response(list_parts_xml(&l.bucket, &l.key, upload_id, &l.parts)),
            None => StatusCode::NOT_FOUND.into_response(),
        });
    }

    let Some((content_type, size, codec, chunks)) = st.engine.object_head(&bucket, &key)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    // Stream chunks out one at a time so a large GET never buffers the whole
    // object; each chunk self-verifies on read.
    let engine = st.engine.clone();
    let body_stream = futures_util::stream::iter(chunks).then(move |h| {
        let engine = engine.clone();
        let codec = codec.clone();
        async move {
            tokio::task::spawn_blocking(move || engine.read_chunk(&h, &codec))
                .await
                .map_err(std::io::Error::other)?
                .map(axum::body::Bytes::from)
                .map_err(std::io::Error::other)
        }
    });

    let ct = HeaderValue::from_str(&content_type)
        .unwrap_or(HeaderValue::from_static(DEFAULT_CONTENT_TYPE));
    Response::builder()
        .header(header::CONTENT_TYPE, ct)
        .header(header::CONTENT_LENGTH, size)
        .body(Body::from_stream(body_stream))
        .map_err(|e| S3Error::Internal(e.to_string()))
}

async fn head_object(
    State(S3State { engine, .. }): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, S3Error> {
    let Some(manifest) = engine.manifest(&bucket, &key)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(manifest.original.size_bytes),
    );
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&manifest.original.content_type)
            .unwrap_or(HeaderValue::from_static(DEFAULT_CONTENT_TYPE)),
    );
    out.insert(header::ETAG, etag(&manifest.object_id.to_string()));
    // Status + headers only; HEAD carries no body.
    Ok((StatusCode::OK, out).into_response())
}

/// DELETE is either an object delete or, with `?uploadId`, an AbortMultipartUpload.
async fn delete_object(
    State(st): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, S3Error> {
    let params = parse_query(query.as_deref());
    if let Some(upload_id) = params.get("uploadId") {
        let engine = st.engine.clone();
        let uid = upload_id.clone();
        tokio::task::spawn_blocking(move || engine.abort_multipart(&uid))
            .await
            .map_err(|e| S3Error::Internal(e.to_string()))??;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    st.engine.delete(&bucket, &key)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- pot (bucket) operations ----

/// CreateBucket. Idempotent: making a pot that already exists is a success, not
/// an error. Persists the pot so a later HeadBucket and ListBuckets see it even
/// while it is empty.
async fn create_bucket(
    State(st): State<S3State>,
    Path(bucket): Path<String>,
) -> Result<Response, S3Error> {
    st.engine.create_bucket(&bucket)?;
    let mut out = HeaderMap::new();
    if let Ok(loc) = HeaderValue::from_str(&format!("/{bucket}")) {
        out.insert(header::LOCATION, loc);
    }
    Ok((StatusCode::OK, out).into_response())
}

/// HeadBucket: 200 if the pot exists (created or written to), 404 otherwise.
async fn head_bucket(
    State(st): State<S3State>,
    Path(bucket): Path<String>,
) -> Result<Response, S3Error> {
    if st.engine.bucket_exists(&bucket)? {
        Ok(StatusCode::OK.into_response())
    } else {
        Ok(StatusCode::NOT_FOUND.into_response())
    }
}

/// DeleteBucket: refuse a non-empty pot (409, matching S3's BucketNotEmpty),
/// otherwise forget its config. Object chunks are reclaimed by GC as usual.
async fn delete_bucket(
    State(st): State<S3State>,
    Path(bucket): Path<String>,
) -> Result<Response, S3Error> {
    if !st.engine.keys(&bucket)?.is_empty() {
        return Ok((StatusCode::CONFLICT, "bucket not empty").into_response());
    }
    st.engine.delete_bucket(&bucket)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// ListBuckets: every pot the store knows, created or written to.
async fn list_buckets(State(st): State<S3State>) -> Result<Response, S3Error> {
    let buckets = st.engine.list_buckets()?;
    Ok(xml_response(list_buckets_xml(&buckets)))
}

/// S3 etags are quoted. Bad chars can't appear in a blake3 id, so this is safe.
fn etag(object_id: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{object_id}\""))
        .unwrap_or(HeaderValue::from_static("\"\""))
}

// ---- query parsing ----

/// Parse a raw query string into a map. A parameter with no `=` (like `uploads`)
/// maps to an empty string, which is enough to test for its presence.
fn parse_query(raw: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(raw) = raw else { return map };
    for pair in raw.split('&').filter(|s| !s.is_empty()) {
        match pair.split_once('=') {
            Some((k, v)) => {
                map.insert(k.to_string(), v.to_string());
            }
            None => {
                map.insert(pair.to_string(), String::new());
            }
        }
    }
    map
}

/// Pull the `<PartNumber>` values out of a CompleteMultipartUpload body, in the
/// order they appear. A tiny hand parser: the body is small and its shape fixed,
/// so this avoids an XML dependency. An empty result tells the engine to fall
/// back to every staged part in ascending order.
fn parse_complete_parts(body: &[u8]) -> Vec<u32> {
    const OPEN: &str = "<PartNumber>";
    const CLOSE: &str = "</PartNumber>";
    let text = String::from_utf8_lossy(body);
    let mut rest = text.as_ref();
    let mut out = Vec::new();
    while let Some(start) = rest.find(OPEN) {
        rest = &rest[start + OPEN.len()..];
        let Some(end) = rest.find(CLOSE) else { break };
        if let Ok(n) = rest[..end].trim().parse::<u32>() {
            out.push(n);
        }
        rest = &rest[end + CLOSE.len()..];
    }
    out
}

// ---- XML responses ----

const XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

fn xml_response(body: String) -> Response {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml"),
    );
    (StatusCode::OK, h, body).into_response()
}

/// Escape the XML text hazards. Pot names and keys can contain `&`, `<`, `>`.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn initiate_xml(bucket: &str, key: &str, upload_id: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <InitiateMultipartUploadResult xmlns=\"{XMLNS}\">\
         <Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId>\
         </InitiateMultipartUploadResult>",
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(upload_id),
    )
}

fn complete_xml(bucket: &str, key: &str, object_id: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <CompleteMultipartUploadResult xmlns=\"{XMLNS}\">\
         <Bucket>{}</Bucket><Key>{}</Key><ETag>\"{}\"</ETag>\
         </CompleteMultipartUploadResult>",
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(object_id),
    )
}

fn list_buckets_xml(buckets: &[String]) -> String {
    // We don't record a per-pot creation time, so CreationDate is a fixed epoch
    // placeholder. S3 clients require the field to be present and well-formed;
    // they don't rely on its value.
    let mut body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListAllMyBucketsResult xmlns=\"{XMLNS}\">\
         <Owner><ID>barme</ID><DisplayName>barme</DisplayName></Owner><Buckets>"
    );
    for b in buckets {
        body.push_str(&format!(
            "<Bucket><Name>{}</Name><CreationDate>1970-01-01T00:00:00.000Z</CreationDate></Bucket>",
            xml_escape(b),
        ));
    }
    body.push_str("</Buckets></ListAllMyBucketsResult>");
    body
}

fn list_parts_xml(bucket: &str, key: &str, upload_id: &str, parts: &[(u32, PartMeta)]) -> String {
    let mut body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListPartsResult xmlns=\"{XMLNS}\">\
         <Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId>",
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(upload_id),
    );
    for (n, meta) in parts {
        body.push_str(&format!(
            "<Part><PartNumber>{n}</PartNumber><ETag>\"{}\"</ETag><Size>{}</Size></Part>",
            xml_escape(&meta.etag),
            meta.size,
        ));
    }
    body.push_str("</ListPartsResult>");
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    // Open-mode state (no credentials), so these tests exercise the routes, not
    // the signing path; SigV4 itself is tested in barme-auth.
    fn state() -> S3State {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the store outlives the test; the OS reclaims it.
        let path = dir.keep();
        S3State {
            engine: Arc::new(Engine::open(path, barme_engine::Policy::default()).unwrap()),
            max_upload_bytes: 512 * 1024 * 1024,
        }
    }

    /// Extract the text between two markers, for reading ids out of XML in tests.
    fn between(haystack: &str, open: &str, close: &str) -> String {
        let start = haystack.find(open).expect("open marker") + open.len();
        let end = haystack[start..].find(close).expect("close marker");
        haystack[start..start + end].to_string()
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let app = app(state());
        let body = b"the bytes go in and the same bytes come out";

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/photos/cat.txt")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from(&body[..]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key(header::ETAG));

        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/photos/cat.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );
        let got = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&got[..], &body[..]);
    }

    #[tokio::test]
    async fn get_unknown_key_is_404() {
        let app = app(state());
        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/photos/nope.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn head_reports_length_without_body() {
        let app = app(state());
        let body = b"measure me";

        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/photos/len.txt")
                    .body(Body::from(&body[..]))
                    .unwrap(),
            )
            .await
            .unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri("/photos/len.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_LENGTH).unwrap(),
            &body.len().to_string()
        );
        let got = res.into_body().collect().await.unwrap().to_bytes();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn delete_then_get_is_404() {
        let app = app(state());
        let body = b"here today";

        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/photos/gone.txt")
                    .body(Body::from(&body[..]))
                    .unwrap(),
            )
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/photos/gone.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/photos/gone.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn multipart_round_trips() {
        let app = app(state());

        // Initiate.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vids/clip.bin?uploads")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let xml = res.into_body().collect().await.unwrap().to_bytes();
        let xml = String::from_utf8_lossy(&xml);
        let upload_id = between(&xml, "<UploadId>", "</UploadId>");

        // Two parts, large enough that each spans several chunks.
        let p1 = vec![b'a'; 200_000];
        let p2 = vec![b'b'; 90_000];
        for (n, data) in [(1u32, &p1), (2u32, &p2)] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(format!("/vids/clip.bin?partNumber={n}&uploadId={upload_id}"))
                        .body(Body::from(data.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            assert!(res.headers().contains_key(header::ETAG));
        }

        // Complete, naming both parts in order.
        let complete = "<CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber></Part>\
             <Part><PartNumber>2</PartNumber></Part>\
             </CompleteMultipartUpload>";
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/vids/clip.bin?uploadId={upload_id}"))
                    .body(Body::from(complete))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // The object reads back as the two parts concatenated.
        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/vids/clip.bin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let got = res.into_body().collect().await.unwrap().to_bytes();
        let mut expected = p1.clone();
        expected.extend_from_slice(&p2);
        assert_eq!(got.len(), expected.len());
        assert_eq!(&got[..], &expected[..]);
    }

    #[tokio::test]
    async fn upload_part_to_unknown_id_is_404() {
        let app = app(state());
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/vids/clip.bin?partNumber=1&uploadId=deadbeef")
                    .body(Body::from(vec![0u8; 10]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    async fn status(app: &axum::Router, method: &str, uri: &str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn create_head_list_delete_bucket() {
        let app = app(state());

        // Unknown pot: HeadBucket is 404.
        assert_eq!(status(&app, "HEAD", "/reports").await, StatusCode::NOT_FOUND);

        // Create it, idempotently.
        assert_eq!(status(&app, "PUT", "/reports").await, StatusCode::OK);
        assert_eq!(status(&app, "PUT", "/reports").await, StatusCode::OK);

        // Now it exists.
        assert_eq!(status(&app, "HEAD", "/reports").await, StatusCode::OK);

        // And it shows up in ListBuckets.
        let res = app
            .clone()
            .oneshot(Request::builder().method("GET").uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let xml = res.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&xml).contains("<Name>reports</Name>"));

        // Empty pot deletes, then reads back as absent.
        assert_eq!(status(&app, "DELETE", "/reports").await, StatusCode::NO_CONTENT);
        assert_eq!(status(&app, "HEAD", "/reports").await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_written_pot_lists_without_being_created() {
        let app = app(state());
        // A first write implies the pot; HeadBucket and ListBuckets both see it.
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/implied/note.txt")
                    .body(Body::from(&b"hi"[..]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status(&app, "HEAD", "/implied").await, StatusCode::OK);
        let res = app
            .oneshot(Request::builder().method("GET").uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let xml = res.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&xml).contains("<Name>implied</Name>"));
    }

    #[tokio::test]
    async fn deleting_a_nonempty_pot_conflicts() {
        let app = app(state());
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/full/doc.txt")
                    .body(Body::from(&b"data"[..]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status(&app, "DELETE", "/full").await, StatusCode::CONFLICT);
    }
}
