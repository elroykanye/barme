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

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, put},
    Router,
};
use barme_engine::{Engine, EngineError};

/// S3 clients expect a Content-Type on every write; use this when they omit one.
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Anything the engine hands back becomes a status + message. Not-found is
/// modelled as an `Option` on the read paths, so it never reaches here.
struct S3Error(EngineError);

impl From<EngineError> for S3Error {
    fn from(e: EngineError) -> Self {
        S3Error(e)
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

/// The router, decoupled from any port so tests can drive it directly.
pub fn app(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/{bucket}/{*key}", put(put_object))
        .route("/{bucket}/{*key}", get(get_object))
        .route("/{bucket}/{*key}", delete(delete_object))
        // HEAD shares the GET route in axum; register it explicitly for clarity.
        .route("/{bucket}/{*key}", axum::routing::head(head_object))
        .with_state(engine)
}

/// Bind and serve until the process ends.
pub async fn serve(engine: Arc<Engine>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app(engine)).await
}

async fn put_object(
    State(engine): State<Arc<Engine>>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE);

    let object_id = engine.put(&bucket, &key, &body, content_type)?;

    let mut out = HeaderMap::new();
    out.insert(header::ETAG, etag(&object_id.to_string()));
    Ok((StatusCode::OK, out).into_response())
}

async fn get_object(
    State(engine): State<Arc<Engine>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, S3Error> {
    let Some(bytes) = engine.get(&bucket, &key)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    // The manifest carries the recorded content-type; fall back if it vanished
    // between the two calls.
    let content_type = engine
        .manifest(&bucket, &key)?
        .map(|m| m.original.content_type)
        .unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_string());

    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static(DEFAULT_CONTENT_TYPE)),
    );
    out.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    Ok((StatusCode::OK, out, bytes).into_response())
}

async fn head_object(
    State(engine): State<Arc<Engine>>,
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
    State(engine): State<Arc<Engine>>,
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

    fn engine() -> Arc<Engine> {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the store outlives the test; the OS reclaims it.
        let path = dir.keep();
        Arc::new(Engine::open(path, barme_engine::Policy::default()).unwrap())
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let app = app(engine());
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
        let app = app(engine());
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
        let app = app(engine());
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
        let app = app(engine());
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
