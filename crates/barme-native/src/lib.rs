//! Native front door: the operations S3 has no vocabulary for.
//!
//!   GET  /history/{bucket}/{*key}   version graph, oldest first
//!   GET  /manifest/{bucket}/{*key}  how the current version was stored
//!   GET  /content/{hash}            fetch any object directly by its id
//!   POST /sync                      tree reconciliation (not yet)
//!   POST /search                    semantic retrieval (not yet)
//!
//! Runs on its own port beside the S3 door, over the same engine, so an object
//! written over S3 can be introspected and diffed here. Paths put the fixed
//! segment first so a wildcard key can't swallow the `/history` suffix.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use barme_core::Hash;
use barme_engine::{Engine, EngineError};

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

struct NativeError(EngineError);

impl From<EngineError> for NativeError {
    fn from(e: EngineError) -> Self {
        NativeError(e)
    }
}

impl IntoResponse for NativeError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

pub fn app(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/history/{bucket}/{*key}", get(history))
        .route("/manifest/{bucket}/{*key}", get(manifest))
        .route("/content/{hash}", get(content))
        .route("/sync", post(not_yet))
        .route("/search", post(not_yet))
        .with_state(engine)
}

pub async fn serve(engine: Arc<Engine>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app(engine)).await
}

/// Every version a key has pointed at, oldest first, as object-id strings.
async fn history(
    State(engine): State<Arc<Engine>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, NativeError> {
    let ids: Vec<String> = engine
        .history(&bucket, &key)?
        .iter()
        .map(|h| h.to_string())
        .collect();
    Ok(Json(ids).into_response())
}

/// The manifest of the current version: codec, fidelity, chunks, quality.
async fn manifest(
    State(engine): State<Arc<Engine>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, NativeError> {
    match engine.manifest(&bucket, &key)? {
        Some(m) => Ok(Json(m).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// Fetch an object by its content address, regardless of what key points at it.
async fn content(
    State(engine): State<Arc<Engine>>,
    Path(hash): Path<String>,
) -> Result<Response, NativeError> {
    let Ok(object_id) = hash.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed content hash").into_response());
    };
    let Some(manifest) = engine.object_manifest(&object_id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let bytes = engine.read_object(&object_id)?;
    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&manifest.original.content_type)
            .unwrap_or(HeaderValue::from_static(DEFAULT_CONTENT_TYPE)),
    );
    out.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    Ok((StatusCode::OK, out, bytes).into_response())
}

/// Placeholder for sync and search. Declared so the shape is visible; the
/// bodies land with the replication and semantic layers.
async fn not_yet(_body: Bytes) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "not implemented yet").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use barme_engine::Policy;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn engine() -> Arc<Engine> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep();
        Arc::new(Engine::open(path, Policy::default()).unwrap())
    }

    async fn get(app: Router, uri: &str) -> Response {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn history_lists_versions_oldest_first() {
        let e = engine();
        let id1 = e.put("b", "k", b"v1", "text/plain").unwrap();
        let id2 = e.put("b", "k", b"v2 is different", "text/plain").unwrap();

        let res = get(app(e), "/history/b/k").await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let ids: Vec<String> = serde_json::from_slice(&body).unwrap();
        assert_eq!(ids, vec![id1.to_string(), id2.to_string()]);
    }

    #[tokio::test]
    async fn manifest_reports_codec_then_404() {
        let e = engine();
        e.put("b", "k", b"hello", "text/plain").unwrap();

        let res = get(app(e.clone()), "/manifest/b/k").await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let m: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(m["storage"]["codec"], "zstd");

        assert_eq!(get(app(e), "/manifest/b/ghost").await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn content_fetches_by_hash() {
        let e = engine();
        let id = e.put("b", "k", b"addressed by content", "text/plain").unwrap();

        let res = get(app(e), &format!("/content/{id}")).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "text/plain");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"addressed by content");
    }

    #[tokio::test]
    async fn unknown_content_hash_is_404_and_junk_is_400() {
        let e = engine();
        let missing = Hash::of(b"never stored");
        assert_eq!(
            get(app(e.clone()), &format!("/content/{missing}")).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(app(e), "/content/not-a-hash").await.status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn sync_and_search_are_not_implemented() {
        let e = engine();
        for path in ["/sync", "/search"] {
            let res = app(e.clone())
                .oneshot(Request::builder().method("POST").uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        }
    }
}
