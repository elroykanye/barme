//! Native front door: the operations S3 has no vocabulary for.
//!
//!   GET  /history/{bucket}/{*key}   version graph, oldest first
//!   GET  /manifest/{bucket}/{*key}  how the current version was stored
//!   GET  /content/{hash}            fetch any object directly by its id
//!   POST /search                    semantic retrieval (if configured)
//!   POST /sync                      tree reconciliation (not yet)
//!
//! Runs on its own port beside the S3 door, over the same engine. Paths put the
//! fixed segment first so a wildcard key can't swallow the `/history` suffix.

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
use barme_semantic::{Semantic, SemanticError};
use serde::Deserialize;

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Shared handler state. Semantic is optional: without it, `/search` reports
/// that it isn't configured rather than failing.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub semantic: Option<Arc<Semantic>>,
}

enum NativeError {
    Engine(EngineError),
    Semantic(SemanticError),
}

impl From<EngineError> for NativeError {
    fn from(e: EngineError) -> Self {
        NativeError::Engine(e)
    }
}

impl From<SemanticError> for NativeError {
    fn from(e: SemanticError) -> Self {
        NativeError::Semantic(e)
    }
}

impl IntoResponse for NativeError {
    fn into_response(self) -> Response {
        let msg = match self {
            NativeError::Engine(e) => e.to_string(),
            NativeError::Semantic(e) => e.to_string(),
        };
        (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
    }
}

/// Router without a semantic layer wired. `/search` will report unconfigured.
pub fn app(engine: Arc<Engine>) -> Router {
    router(AppState {
        engine,
        semantic: None,
    })
}

/// Router with semantic search enabled.
pub fn app_with_semantic(engine: Arc<Engine>, semantic: Arc<Semantic>) -> Router {
    router(AppState {
        engine,
        semantic: Some(semantic),
    })
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/history/{bucket}/{*key}", get(history))
        .route("/manifest/{bucket}/{*key}", get(manifest))
        .route("/content/{hash}", get(content))
        .route("/search", post(search))
        .route("/sync", post(not_yet))
        .with_state(state)
}

pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await
}

async fn history(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, NativeError> {
    let ids: Vec<String> = st
        .engine
        .history(&bucket, &key)?
        .iter()
        .map(|h| h.to_string())
        .collect();
    Ok(Json(ids).into_response())
}

async fn manifest(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, NativeError> {
    match st.engine.manifest(&bucket, &key)? {
        Some(m) => Ok(Json(m).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn content(
    State(st): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response, NativeError> {
    let Ok(object_id) = hash.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed content hash").into_response());
    };
    let Some(manifest) = st.engine.object_manifest(&object_id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let bytes = st.engine.read_object(&object_id)?;
    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&manifest.original.content_type)
            .unwrap_or(HeaderValue::from_static(DEFAULT_CONTENT_TYPE)),
    );
    out.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    Ok((StatusCode::OK, out, bytes).into_response())
}

fn default_k() -> usize {
    10
}

fn default_tenant() -> String {
    "default".to_string()
}

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default = "default_k")]
    k: usize,
    #[serde(default = "default_tenant")]
    tenant: String,
}

/// Embed the query text and return the nearest objects. The semantic check runs
/// before the body is parsed so an unconfigured instance answers 501 cleanly.
async fn search(State(st): State<AppState>, body: Bytes) -> Result<Response, NativeError> {
    let Some(semantic) = &st.semantic else {
        return Ok((StatusCode::NOT_IMPLEMENTED, "semantic search not configured").into_response());
    };
    let Ok(req) = serde_json::from_slice::<SearchRequest>(&body) else {
        return Ok((StatusCode::BAD_REQUEST, "expected {query, k?, tenant?}").into_response());
    };

    let hits = semantic
        .search(&req.tenant, req.query.as_bytes(), "text/plain", req.k)
        .await?;
    let out: Vec<serde_json::Value> = hits
        .iter()
        .map(|m| serde_json::json!({ "id": m.id.to_string(), "score": m.score }))
        .collect();
    Ok(Json(out).into_response())
}

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

        assert_eq!(
            get(app(e), "/manifest/b/ghost").await.status(),
            StatusCode::NOT_FOUND
        );
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
    async fn search_without_semantic_is_501() {
        let e = engine();
        for path in ["/sync", "/search"] {
            let res = app(e.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        }
    }
}
