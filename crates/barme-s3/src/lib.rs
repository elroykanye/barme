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
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Router,
};
use barme_auth::{authorize, verify_sigv4, Action, Credentials, Principal, SignedRequest};
use barme_engine::{Engine, EngineError, ObjectPage, PartMeta};
use futures_util::{StreamExt, TryStreamExt};
use tokio_util::io::{StreamReader, SyncIoBridge};

/// S3 clients expect a Content-Type on every write; use this when they omit one.
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Cap on the CompleteMultipartUpload request body. It only carries a part list;
/// 10k parts at ~120 bytes each is ~1.2 MiB, so 16 MiB is comfortable headroom.
const MAX_COMPLETE_BODY: usize = 16 * 1024 * 1024;

/// Most keys one ListObjectsV2 page may return. S3's own ceiling, and what
/// clients assume when they page; a larger `max-keys` is silently clamped to it
/// rather than refused, which is also what S3 does.
const MAX_KEYS_LIMIT: usize = 1000;

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
        .route("/{bucket}", get(list_objects_v2))
        // Clients differ on whether the pot path carries a trailing slash, and
        // axum doesn't fold one into the other, so listing answers on both.
        .route("/{bucket}/", get(list_objects_v2))
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
    let (n, v) = object_id_header(&object_id.to_string());
    out.insert(n, v);
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
        // Surface the object id in a header too: the completion ETag is XML-body
        // only, and a client building a /cdn link needs the hash the same way it
        // gets one from a single PUT.
        let mut resp = xml_response(complete_xml(&bucket, &key, &object_id.to_string()));
        let (n, v) = object_id_header(&object_id.to_string());
        resp.headers_mut().insert(n, v);
        return Ok(resp);
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
    let (n, v) = object_id_header(&manifest.object_id.to_string());
    out.insert(n, v);
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
/// otherwise forget its config. Object chunks are reclaimed by GC as usual. The
/// emptiness check and the delete are atomic (see `delete_bucket_if_empty`), so a
/// PUT racing the delete can't have its just-acknowledged object silently wiped.
/// Runs on a blocking task because it briefly holds the commit locks.
async fn delete_bucket(
    State(st): State<S3State>,
    Path(bucket): Path<String>,
) -> Result<Response, S3Error> {
    let engine = st.engine.clone();
    let deleted = tokio::task::spawn_blocking(move || engine.delete_bucket_if_empty(&bucket))
        .await
        .map_err(|e| S3Error::Internal(e.to_string()))??;
    if deleted {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((StatusCode::CONFLICT, "bucket not empty").into_response())
    }
}

/// ListBuckets: every pot the store knows, created or written to.
async fn list_buckets(State(st): State<S3State>) -> Result<Response, S3Error> {
    let buckets = st.engine.list_buckets()?;
    Ok(xml_response(list_buckets_xml(&buckets)))
}

/// The ListObjectsV2 request, decoded: what to list, and what the response has
/// to echo back to the client.
struct ListQuery {
    prefix: String,
    delimiter: String,
    max_keys: usize,
    /// The token the client sent, echoed back verbatim. `None` on a first page.
    continuation_token: Option<String>,
    /// `start-after`, echoed back verbatim.
    start_after: Option<String>,
    /// The key to resume strictly after, from the token or from `start-after`.
    after: Option<String>,
    /// `encoding-type=url`: URL-encode keys and prefixes in the response, so a
    /// key holding a character XML can't carry still round-trips.
    url_encode: bool,
}

impl ListQuery {
    fn parse(params: &HashMap<String, String>) -> Result<Self, S3Error> {
        let value = |k: &str| params.get(k).map(|v| percent_decode(v));

        let max_keys = match params.get("max-keys") {
            None => MAX_KEYS_LIMIT,
            Some(raw) => raw
                .parse::<usize>()
                .map_err(|_| S3Error::BadRequest("max-keys must be a non-negative integer".into()))?
                .min(MAX_KEYS_LIMIT),
        };

        let continuation_token = value("continuation-token").filter(|t| !t.is_empty());
        let start_after = value("start-after").filter(|k| !k.is_empty());

        // A token is our own opaque handle: the last key of the previous page,
        // hex-encoded so an arbitrary key survives a round trip through a query
        // string. When both are present the token wins, as it does in S3 — the
        // client is mid-pagination and `start-after` is a stale first-page hint.
        let after = match &continuation_token {
            Some(t) => Some(decode_continuation_token(t)?),
            None => start_after.clone(),
        };

        let encoding_type = value("encoding-type").unwrap_or_default();
        if !encoding_type.is_empty() && encoding_type != "url" {
            return Err(S3Error::BadRequest(
                "encoding-type must be url if given".into(),
            ));
        }

        Ok(ListQuery {
            prefix: value("prefix").unwrap_or_default(),
            delimiter: value("delimiter").unwrap_or_default(),
            max_keys,
            continuation_token,
            start_after,
            after,
            url_encode: encoding_type == "url",
        })
    }

    /// Apply `encoding-type` to one key or prefix on the way out.
    fn encode(&self, s: &str) -> String {
        if self.url_encode {
            uri_encode(s)
        } else {
            s.to_string()
        }
    }
}

/// ListObjectsV2 (`GET /{pot}?list-type=2`): one page of a pot's keys, with the
/// prefix, delimiter and continuation token S3 tooling pages by.
///
/// Only v2 is served. The v1 form (`GET /{pot}` with `marker`) answers 501
/// rather than pretending: a client that got an empty v2-shaped body back would
/// read it as "the pot is empty" and a mirror would happily copy nothing.
async fn list_objects_v2(
    State(st): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, S3Error> {
    let params = parse_query(query.as_deref());
    if params.get("list-type").map(String::as_str) != Some("2") {
        return Ok((
            StatusCode::NOT_IMPLEMENTED,
            "only ListObjectsV2 is supported; send list-type=2",
        )
            .into_response());
    }

    if !st.engine.bucket_exists(&bucket)? {
        return Ok((StatusCode::NOT_FOUND, "no such pot").into_response());
    }

    let q = ListQuery::parse(&params)?;

    // A page reads a directory and up to `max_keys` manifests, so it goes on a
    // blocking task like the other filesystem-heavy handlers.
    let engine = st.engine.clone();
    let (b, prefix, delimiter, after, max) = (
        bucket.clone(),
        q.prefix.clone(),
        q.delimiter.clone(),
        q.after.clone(),
        q.max_keys,
    );
    let page = tokio::task::spawn_blocking(move || {
        engine.list_objects(&b, &prefix, &delimiter, after.as_deref(), max)
    })
    .await
    .map_err(|e| S3Error::Internal(e.to_string()))??;

    Ok(xml_response(list_objects_xml(&bucket, &q, &page)))
}

/// Header carrying the object's own content id (its blake3) — the handle for a
/// `/cdn/{hash}` link. Set on both write paths (single PUT and multipart
/// complete) and on HEAD, so a client gets the same hash regardless of how the
/// object was written. The S3 ETag can't be relied on for this: a multipart
/// ETag is a digest-of-digests, not the object hash. See issue #7.
const OBJECT_ID_HEADER: &str = "x-barme-object-id";

fn object_id_header(id: &str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static(OBJECT_ID_HEADER),
        HeaderValue::from_str(id).unwrap_or(HeaderValue::from_static("")),
    )
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

/// Percent-decode a query-string value.
///
/// `+` is left as a literal plus, not turned into a space: SigV4 canonicalizes
/// query strings with `%20`, so a `+` in a signed request is a real plus, and a
/// prefix like `photos/holiday+2026/` would otherwise silently match nothing.
/// A stray `%` that isn't followed by two hex digits is kept as written rather
/// than swallowed.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(&hi), Some(&lo)) => match (hex_nibble(hi), hex_nibble(lo)) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode for `encoding-type=url`, AWS's rules: only the unreserved set
/// passes through, so `/` is encoded too. That's the point of the option — a key
/// can legally hold bytes XML has no way to represent, and encoding is how the
/// response stays parseable; the SDK decodes on the way back.
fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Encode the cursor into an opaque continuation token. Keys can hold `&`, `=`
/// and worse, so the token is hex: it survives any query string, and clients
/// treat it as opaque anyway.
fn encode_continuation_token(key: &str) -> String {
    hex::encode(key.as_bytes())
}

fn decode_continuation_token(token: &str) -> Result<String, S3Error> {
    let bytes = hex::decode(token)
        .map_err(|_| S3Error::BadRequest("continuation-token is not a token we issued".into()))?;
    String::from_utf8(bytes)
        .map_err(|_| S3Error::BadRequest("continuation-token is not a token we issued".into()))
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

/// S3's LastModified: UTC, milliseconds, `Z`. Manifests record RFC3339, which is
/// close but allows a non-UTC offset and any number of sub-second digits, so it
/// is read back and reformatted rather than passed through. A timestamp that
/// won't parse falls back to the epoch: the field is required and must be
/// well-formed for a client to parse the page at all, so one odd manifest must
/// not take the whole listing down with it.
fn s3_timestamp(rfc3339: &str) -> String {
    use time::format_description::well_known::Rfc3339;
    let Ok(t) = time::OffsetDateTime::parse(rfc3339, &Rfc3339) else {
        return "1970-01-01T00:00:00.000Z".to_string();
    };
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond(),
    )
}

/// The ListObjectsV2 response body.
///
/// `KeyCount` counts entries *and* collapsed prefixes, the way S3 counts them,
/// so it always matches what `MaxKeys` bounded. `NextContinuationToken` appears
/// only when keys remain, and `IsTruncated` is exactly that condition — a client
/// stops paging on `false`, so the two must never disagree.
fn list_objects_xml(bucket: &str, q: &ListQuery, page: &ObjectPage) -> String {
    let mut body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"{XMLNS}\">\
         <Name>{}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount>\
         <MaxKeys>{}</MaxKeys><IsTruncated>{}</IsTruncated>",
        xml_escape(bucket),
        xml_escape(&q.encode(&q.prefix)),
        page.entries.len() + page.common_prefixes.len(),
        q.max_keys,
        page.next_after.is_some(),
    );
    if !q.delimiter.is_empty() {
        body.push_str(&format!(
            "<Delimiter>{}</Delimiter>",
            xml_escape(&q.encode(&q.delimiter)),
        ));
    }
    if q.url_encode {
        body.push_str("<EncodingType>url</EncodingType>");
    }
    // Echo the cursor the client sent, so it can match a response to a request.
    if let Some(t) = &q.continuation_token {
        body.push_str(&format!(
            "<ContinuationToken>{}</ContinuationToken>",
            xml_escape(t),
        ));
    }
    if let Some(k) = &q.start_after {
        body.push_str(&format!(
            "<StartAfter>{}</StartAfter>",
            xml_escape(&q.encode(k)),
        ));
    }
    if let Some(next) = &page.next_after {
        body.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            encode_continuation_token(next),
        ));
    }
    for o in &page.entries {
        body.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>{}</LastModified>\
             <ETag>\"{}\"</ETag><Size>{}</Size>\
             <StorageClass>STANDARD</StorageClass></Contents>",
            xml_escape(&q.encode(&o.key)),
            s3_timestamp(&o.created_at),
            xml_escape(&o.object_id.to_string()),
            o.size,
        ));
    }
    for p in &page.common_prefixes {
        body.push_str(&format!(
            "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
            xml_escape(&q.encode(p)),
        ));
    }
    body.push_str("</ListBucketResult>");
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

    fn state_with_auth() -> S3State {
        let state = state();
        state.engine.ensure_owner(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        ).unwrap();
        state
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
    async fn botocore_presigned_get_downloads_a_private_object() {
        let state = state_with_auth();
        let body = b"downloaded through a standard SDK presign";
        state
            .engine
            .put("photos", "cat.txt", body, "text/plain")
            .unwrap();
        let query = concat!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&",
            "X-Amz-Credential=AKIDEXAMPLE%2F20260817%2Fus-east-1%2Fs3%2Faws4_request&",
            "X-Amz-Date=20260817T120000Z&",
            "X-Amz-Expires=900&",
            "X-Amz-SignedHeaders=host&",
            "X-Amz-Signature=66b10d4b3e938da23092182bff453f832540bd89f9e6e64324ca5927167faba2",
        );

        let res = app(state)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/photos/cat.txt?{query}"))
                    .header(header::HOST, "barme.local")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let got = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&got[..], body);
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

    // ---- ListObjectsV2 ----

    /// Read a body as text, for poking at XML.
    async fn body_text(res: Response) -> String {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Every occurrence of the text between two markers, in order.
    fn all_between(haystack: &str, open: &str, close: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = haystack;
        while let Some(start) = rest.find(open) {
            rest = &rest[start + open.len()..];
            let Some(end) = rest.find(close) else { break };
            out.push(rest[..end].to_string());
            rest = &rest[end + close.len()..];
        }
        out
    }

    async fn put(app: &axum::Router, uri: &str, body: &[u8]) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .body(Body::from(body.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    async fn list(app: &axum::Router, uri: &str) -> String {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
        body_text(res).await
    }

    #[tokio::test]
    async fn list_v2_returns_keys_with_size_and_etag() {
        let app = app(state());
        put(&app, "/photos/a.txt", b"aa").await;
        put(&app, "/photos/b.txt", b"bbbb").await;

        let xml = list(&app, "/photos?list-type=2").await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["a.txt", "b.txt"]);
        assert_eq!(all_between(&xml, "<Size>", "</Size>"), ["2", "4"]);
        assert!(xml.contains("<Name>photos</Name>"));
        assert!(xml.contains("<KeyCount>2</KeyCount>"));
        assert!(xml.contains("<IsTruncated>false</IsTruncated>"));
        // A well-formed LastModified is what an SDK's strict parser needs.
        for ts in all_between(&xml, "<LastModified>", "</LastModified>") {
            assert!(ts.ends_with('Z'), "not UTC-with-Z: {ts}");
            assert_eq!(ts.len(), "1970-01-01T00:00:00.000Z".len(), "{ts}");
        }
        // The ETag is the object id, the same handle HEAD reports.
        for etag in all_between(&xml, "<ETag>", "</ETag>") {
            assert!(etag.starts_with("\"blake3:"), "{etag}");
        }
    }

    #[tokio::test]
    async fn list_v2_answers_on_the_trailing_slash_form_too() {
        let app = app(state());
        put(&app, "/photos/a.txt", b"aa").await;
        let xml = list(&app, "/photos/?list-type=2").await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["a.txt"]);
    }

    #[tokio::test]
    async fn list_v2_of_an_empty_pot_is_an_empty_page_not_a_404() {
        let app = app(state());
        assert_eq!(status(&app, "PUT", "/fresh").await, StatusCode::OK);
        let xml = list(&app, "/fresh?list-type=2").await;
        assert!(xml.contains("<KeyCount>0</KeyCount>"));
        assert!(!xml.contains("<Contents>"));
    }

    #[tokio::test]
    async fn list_v2_of_an_unknown_pot_is_404() {
        let app = app(state());
        assert_eq!(
            status(&app, "GET", "/ghost?list-type=2").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_bare_get_on_the_pot_path_is_not_implemented() {
        let app = app(state());
        put(&app, "/photos/a.txt", b"aa").await;
        // v1 ListObjects. Answering 501 beats handing back a v2 body that a v1
        // client would read as an empty pot.
        assert_eq!(
            status(&app, "GET", "/photos").await,
            StatusCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn list_v2_filters_by_prefix_and_groups_by_delimiter() {
        let app = app(state());
        for key in ["logs/2026/a", "logs/2026/b", "photos/c", "readme"] {
            put(&app, &format!("/pot/{key}"), b"x").await;
        }

        let xml = list(&app, "/pot?list-type=2&delimiter=%2F").await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["readme"]);
        assert_eq!(
            all_between(&xml, "<CommonPrefixes><Prefix>", "</Prefix>"),
            ["logs/", "photos/"]
        );

        // A prefix that itself needs decoding, and grouping under it.
        let xml = list(&app, "/pot?list-type=2&prefix=logs%2F&delimiter=%2F").await;
        assert!(all_between(&xml, "<Key>", "</Key>").is_empty());
        assert_eq!(
            all_between(&xml, "<CommonPrefixes><Prefix>", "</Prefix>"),
            ["logs/2026/"]
        );
    }

    #[tokio::test]
    async fn paging_by_continuation_token_returns_every_key_exactly_once() {
        let app = app(state());
        let expected: Vec<String> = (0..12).map(|i| format!("k{i:02}")).collect();
        for key in &expected {
            put(&app, &format!("/pot/{key}"), b"x").await;
        }

        // Page at 5, the way a mirroring tool would: follow the token until the
        // server stops issuing one.
        let mut seen: Vec<String> = Vec::new();
        let mut uri = "/pot?list-type=2&max-keys=5".to_string();
        let mut pages = 0;
        loop {
            let xml = list(&app, &uri).await;
            seen.extend(all_between(&xml, "<Key>", "</Key>"));
            pages += 1;
            assert!(pages <= 5, "paging did not terminate");
            let next = all_between(&xml, "<NextContinuationToken>", "</NextContinuationToken>");
            match next.first() {
                Some(token) => {
                    assert!(xml.contains("<IsTruncated>true</IsTruncated>"));
                    uri = format!("/pot?list-type=2&max-keys=5&continuation-token={token}");
                }
                None => {
                    assert!(xml.contains("<IsTruncated>false</IsTruncated>"));
                    break;
                }
            }
        }
        assert_eq!(pages, 3); // 5 + 5 + 2
        assert_eq!(seen, expected);
    }

    #[tokio::test]
    async fn a_token_survives_a_key_holding_query_string_punctuation() {
        let app = app(state());
        // `&` and `=` in a key would tear a raw cursor in half on the way back.
        for key in ["a&b=c", "z"] {
            put(&app, &format!("/pot/{key}"), b"x").await;
        }
        let xml = list(&app, "/pot?list-type=2&max-keys=1").await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["a&amp;b=c"]);
        let token = all_between(&xml, "<NextContinuationToken>", "</NextContinuationToken>")
            .pop()
            .expect("a truncated page must issue a token");

        let xml = list(
            &app,
            &format!("/pot?list-type=2&max-keys=1&continuation-token={token}"),
        )
        .await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["z"]);
    }

    #[tokio::test]
    async fn start_after_resumes_past_a_key() {
        let app = app(state());
        for key in ["a", "b", "c"] {
            put(&app, &format!("/pot/{key}"), b"x").await;
        }
        let xml = list(&app, "/pot?list-type=2&start-after=b").await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["c"]);
        assert!(xml.contains("<StartAfter>b</StartAfter>"));
    }

    #[tokio::test]
    async fn max_keys_over_the_ceiling_is_clamped_not_refused() {
        let app = app(state());
        put(&app, "/pot/a", b"x").await;
        let xml = list(&app, "/pot?list-type=2&max-keys=99999").await;
        assert!(xml.contains(&format!("<MaxKeys>{MAX_KEYS_LIMIT}</MaxKeys>")));
    }

    #[tokio::test]
    async fn a_malformed_query_is_a_400_not_a_500() {
        let app = app(state());
        put(&app, "/pot/a", b"x").await;
        for uri in [
            "/pot?list-type=2&max-keys=lots",
            "/pot?list-type=2&encoding-type=rot13",
            // Not hex, so it was never a token we issued.
            "/pot?list-type=2&continuation-token=zzz",
        ] {
            assert_eq!(status(&app, "GET", uri).await, StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    #[tokio::test]
    async fn encoding_type_url_escapes_keys_xml_could_not_carry() {
        let app = app(state());
        put(&app, "/pot/a%20b.txt", b"x").await;
        let xml = list(&app, "/pot?list-type=2&encoding-type=url").await;
        assert!(xml.contains("<EncodingType>url</EncodingType>"));
        // `/` is encoded too, per AWS's rules, which is what SDKs decode.
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["a%20b.txt"]);
    }

    #[tokio::test]
    async fn a_plus_in_a_prefix_stays_a_plus() {
        let app = app(state());
        put(&app, "/pot/holiday+2026/a", b"x").await;
        put(&app, "/pot/other/b", b"x").await;
        // Decoding `+` to a space here would match nothing at all.
        let xml = list(&app, "/pot?list-type=2&prefix=holiday%2B2026%2F").await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["holiday+2026/a"]);
    }

    #[tokio::test]
    async fn a_literal_percent_in_a_key_is_encoded_not_passed_through() {
        let app = app(state());
        // `%25` in the request URI, so the key that lands is `100%-done.txt`.
        put(&app, "/pot/100%25-done.txt", b"x").await;

        // This is why honouring encoding-type isn't cosmetic. botocore sets it on
        // every list and percent-decodes the reply, so a raw `%` here would come
        // back through the SDK mangled — or throw. Encoded, it round-trips.
        let xml = list(&app, "/pot?list-type=2&encoding-type=url").await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["100%25-done.txt"]);

        // Without the parameter the key is reported as it is stored.
        let xml = list(&app, "/pot?list-type=2").await;
        assert_eq!(all_between(&xml, "<Key>", "</Key>"), ["100%-done.txt"]);
    }

    #[tokio::test]
    async fn a_key_with_xml_punctuation_comes_back_escaped() {
        let app = app(state());
        // Escaped in the request URI because `<`/`>` aren't legal there either;
        // the key that lands is the decoded `a&b<c>.txt`.
        put(&app, "/pot/a%26b%3Cc%3E.txt", b"x").await;
        let xml = list(&app, "/pot?list-type=2").await;
        assert!(xml.contains("<Key>a&amp;b&lt;c&gt;.txt</Key>"), "{xml}");
    }

    #[tokio::test]
    async fn object_id_header_is_consistent_across_write_paths_and_head() {
        let app = app(state());
        let hdr = |res: &Response| {
            res.headers()
                .get("x-barme-object-id")
                .map(|v| v.to_str().unwrap().to_string())
        };

        // Single PUT surfaces the object id (the /cdn handle).
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/pot/small.bin")
                    .body(Body::from(&b"tiny"[..]))
                    .unwrap(),
            )
            .await
            .unwrap();
        let put_id = hdr(&res).expect("PUT must carry x-barme-object-id");
        assert!(put_id.starts_with("blake3:"));

        // HEAD returns the same id.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri("/pot/small.bin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hdr(&res).as_deref(), Some(put_id.as_str()));

        // Multipart complete carries it too — the whole point of #7, since the
        // multipart ETag isn't the object hash.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/pot/big.bin?uploads")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let xml = res.into_body().collect().await.unwrap().to_bytes();
        let uid = between(&String::from_utf8_lossy(&xml), "<UploadId>", "</UploadId>");
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/pot/big.bin?partNumber=1&uploadId={uid}"))
                    .body(Body::from(vec![7u8; 100_000]))
                    .unwrap(),
            )
            .await
            .unwrap();
        let complete = "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber></Part></CompleteMultipartUpload>";
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/pot/big.bin?uploadId={uid}"))
                    .body(Body::from(complete))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let mp_id = hdr(&res).expect("multipart complete must carry x-barme-object-id");
        assert!(mp_id.starts_with("blake3:"));

        // HEAD on the multipart object returns the same handle.
        let res = app
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri("/pot/big.bin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hdr(&res).as_deref(), Some(mp_id.as_str()));
    }
}
