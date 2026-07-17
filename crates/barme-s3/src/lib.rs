//! S3-compatible front door.
//!
//! bucket/key/object maps almost directly onto bucket/pointer/manifest:
//!   PUT    -> engine write path
//!   GET    -> engine read path
//!   DELETE -> move/clear pointer (chunks reclaimed later by GC, never inline)
//!   HEAD   -> manifest lookup; etag is the content hash
//!   List   -> list pointers
//!   multipart, presigned URLs
//!
//! Scope: the parts real clients use first. The long tail of bucket
//! sub-resources (ACLs, lifecycle, policies) comes later.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, put},
    Router,
};
use barme_auth::{authorize, verify_sigv4, Action, Credentials, Principal, SignedRequest};
use barme_engine::{Engine, EngineError};
use futures_util::{StreamExt, TryStreamExt};
use tokio_util::io::{StreamReader, SyncIoBridge};

/// S3 clients expect a Content-Type on every write; use this when they omit one.
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Anything the engine hands back becomes a status + message. Not-found is
/// modelled as an `Option` on the read paths, so it never reaches here.
enum S3Error {
    Engine(EngineError),
    /// The upload's blocking task failed to run (panic or cancellation).
    Internal(String),
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
            S3Error::Engine(e) if e.is_bad_input() => (StatusCode::BAD_REQUEST, e.to_string()),
            S3Error::Engine(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            S3Error::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
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
        .route("/{bucket}/{*key}", put(put_object))
        .route("/{bucket}/{*key}", get(get_object))
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

async fn put_object(
    State(st): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
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

async fn get_object(
    State(st): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, S3Error> {
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

async fn delete_object(
    State(S3State { engine, .. }): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, S3Error> {
    engine.delete(&bucket, &key)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// S3 etags are quoted. Bad chars can't appear in a blake3 id, so this is safe.
fn etag(object_id: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{object_id}\""))
        .unwrap_or(HeaderValue::from_static("\"\""))
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
}
