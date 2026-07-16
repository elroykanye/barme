//! Native front door. Two jobs: the operations S3 can't express (version
//! history, fetch-by-hash, sync, search), and a browser-friendly app API for a
//! frontend, so a React app can do everything over JSON + Basic auth without
//! signing SigV4 requests.
//!
//! Auth: Basic (access:secret). With no credentials configured the door runs
//! open. Reads obey bucket visibility; writes, deletes, listing, search, and
//! fetch-by-hash require the owner. CORS is permissive so a browser can call in.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use barme_auth::{authorize, Action, Credentials};
use barme_core::{BucketConfig, Hash, KeyRecord};
use barme_engine::{Engine, EngineError};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub semantic: Option<Arc<barme_semantic::Semantic>>,
}

enum NativeError {
    Engine(EngineError),
    Semantic(barme_semantic::SemanticError),
    Forbidden,
}

impl From<EngineError> for NativeError {
    fn from(e: EngineError) -> Self {
        NativeError::Engine(e)
    }
}
impl From<barme_semantic::SemanticError> for NativeError {
    fn from(e: barme_semantic::SemanticError) -> Self {
        NativeError::Semantic(e)
    }
}

impl IntoResponse for NativeError {
    fn into_response(self) -> Response {
        match self {
            NativeError::Engine(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
            NativeError::Semantic(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
            NativeError::Forbidden => {
                (StatusCode::FORBIDDEN, "access denied").into_response()
            }
        }
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/stats", get(stats))
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/{access}", delete(delete_key))
        .route("/pots", get(list_buckets))
        .route("/pots/{bucket}", delete(delete_bucket))
        .route("/pots/{bucket}/rename", post(rename_bucket))
        .route("/pots/{bucket}/visibility", post(set_visibility))
        .route("/pots/{bucket}/config", get(get_config).put(put_config))
        .route("/pots/{bucket}/objects", get(list_objects))
        .route("/ops/copy", post(copy_object))
        .route("/ops/move", post(move_object))
        .route("/objects/{bucket}/{*key}", get(download).put(upload).delete(remove))
        .route("/history/{bucket}/{*key}", get(history))
        .route("/manifest/{bucket}/{*key}", get(manifest))
        .route("/content/{hash}", get(content))
        .route("/search", post(search))
        .route("/sync", post(not_yet))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app(state)).await
}

// ---- auth helpers --------------------------------------------------------

/// A resolved caller for one request. `open` means no keys are configured
/// (auth disabled); otherwise `record` is the authenticated key, if any.
struct Caller {
    open: bool,
    record: Option<KeyRecord>,
}

fn caller(state: &AppState, headers: &HeaderMap) -> Caller {
    let keys = state.engine.list_keys().unwrap_or_default();
    if keys.is_empty() {
        return Caller { open: true, record: None };
    }
    let creds = Credentials::from_records(keys);
    let record = basic(headers).and_then(|(access, secret)| match creds.record(&access) {
        Some(r) if r.secret_key == secret => Some(r.clone()),
        _ => None,
    });
    Caller { open: false, record }
}

fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Basic "))
        .and_then(|b64| STANDARD.decode(b64.trim()).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())?;
    let (access, secret) = raw.split_once(':')?;
    Some((access.to_string(), secret.to_string()))
}

impl Caller {
    fn require_owner(&self) -> Result<(), NativeError> {
        if self.open || self.record.as_ref().map(|k| k.is_owner()).unwrap_or(false) {
            Ok(())
        } else {
            Err(NativeError::Forbidden)
        }
    }
    fn require_write(&self, pot: &str) -> Result<(), NativeError> {
        if self.open || authorize(self.record.as_ref(), Action::Write, pot, false) {
            Ok(())
        } else {
            Err(NativeError::Forbidden)
        }
    }
    fn require_read(&self, state: &AppState, pot: &str) -> Result<(), NativeError> {
        let public = state.engine.is_public(pot).unwrap_or(false);
        if self.open || authorize(self.record.as_ref(), Action::Read, pot, public) {
            Ok(())
        } else {
            Err(NativeError::Forbidden)
        }
    }
}

// ---- bucket + object listing --------------------------------------------

#[derive(Serialize)]
struct BucketInfo {
    name: String,
    public_read: bool,
    objects: usize,
}

async fn stats(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    Ok(Json(st.engine.stats()?).into_response())
}

async fn list_keys(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    // Never return secrets in a listing.
    let keys: Vec<serde_json::Value> = st
        .engine
        .list_keys()?
        .iter()
        .map(|k| {
            serde_json::json!({
                "access_key": k.access_key,
                "read_only": k.read_only,
                "pots": k.pots,
                "created_at": k.created_at,
            })
        })
        .collect();
    Ok(Json(keys).into_response())
}

#[derive(Deserialize)]
struct NewKey {
    access_key: String,
    secret_key: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    pots: Vec<String>,
}

async fn create_key(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(nk): Json<NewKey>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    st.engine.create_key(&KeyRecord {
        access_key: nk.access_key,
        secret_key: nk.secret_key,
        read_only: nk.read_only,
        pots: nk.pots,
        created_at: String::new(),
    })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_key(
    State(st): State<AppState>,
    Path(access): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    st.engine.delete_key(&access)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_buckets(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let mut out = Vec::new();
    for name in st.engine.buckets()? {
        out.push(BucketInfo {
            public_read: st.engine.is_public(&name)?,
            objects: st.engine.keys(&name)?.len(),
            name,
        });
    }
    Ok(Json(out).into_response())
}

#[derive(Serialize)]
struct ObjectInfo {
    key: String,
    size: u64,
    versions: usize,
}

async fn list_objects(
    State(st): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    let mut out = Vec::new();
    for key in st.engine.keys(&bucket)? {
        let size = st
            .engine
            .manifest(&bucket, &key)?
            .map(|m| m.original.size_bytes)
            .unwrap_or(0);
        out.push(ObjectInfo {
            versions: st.engine.history(&bucket, &key)?.len(),
            key,
            size,
        });
    }
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
struct Visibility {
    public_read: bool,
}

async fn set_visibility(
    State(st): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Json(vis): Json<Visibility>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    // Preserve the rest of the pot's config; only flip visibility.
    let mut cfg = st.engine.bucket_config(&bucket)?;
    cfg.public_read = vis.public_read;
    st.engine.set_bucket_config(&bucket, &cfg)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn get_config(
    State(st): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    Ok(Json(st.engine.bucket_config(&bucket)?).into_response())
}

async fn put_config(
    State(st): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Json(cfg): Json<BucketConfig>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    st.engine.set_bucket_config(&bucket, &cfg)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct RenameBucket {
    new_name: String,
}

async fn rename_bucket(
    State(st): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RenameBucket>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    st.engine.rename_bucket(&bucket, &body.new_name)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_bucket(
    State(st): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    st.engine.delete_bucket(&bucket)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct MoveCopy {
    from_bucket: String,
    from_key: String,
    to_bucket: String,
    to_key: String,
}

async fn copy_object(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(m): Json<MoveCopy>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let ok = st
        .engine
        .copy_object(&m.from_bucket, &m.from_key, &m.to_bucket, &m.to_key)?;
    Ok((if ok { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }).into_response())
}

async fn move_object(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(m): Json<MoveCopy>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let ok = st
        .engine
        .move_object(&m.from_bucket, &m.from_key, &m.to_bucket, &m.to_key)?;
    Ok((if ok { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }).into_response())
}

// ---- object CRUD ---------------------------------------------------------

#[derive(Serialize)]
struct Uploaded {
    object_id: String,
}

async fn upload(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_write(&bucket)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE);
    let object_id = st.engine.put(&bucket, &key, &body, content_type)?;
    Ok(Json(Uploaded {
        object_id: object_id.to_string(),
    })
    .into_response())
}

async fn download(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    let Some(bytes) = st.engine.get(&bucket, &key)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let content_type = st
        .engine
        .manifest(&bucket, &key)?
        .map(|m| m.original.content_type)
        .unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_string());
    Ok(with_bytes(&content_type, bytes))
}

async fn remove(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_write(&bucket)?;
    st.engine.delete(&bucket, &key)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- introspection -------------------------------------------------------

async fn history(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
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
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    match st.engine.manifest(&bucket, &key)? {
        Some(m) => Ok(Json(m).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn content(
    State(st): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    // Fetch-by-hash isn't bucket-scoped, so it's owner-only.
    caller(&st, &headers).require_owner()?;
    let Ok(object_id) = hash.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed content hash").into_response());
    };
    let Some(m) = st.engine.object_manifest(&object_id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let bytes = st.engine.read_object(&object_id)?;
    Ok(with_bytes(&m.original.content_type, bytes))
}

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default = "default_k")]
    k: usize,
    #[serde(default = "default_tenant")]
    tenant: String,
}

fn default_k() -> usize {
    10
}
fn default_tenant() -> String {
    "default".to_string()
}

async fn search(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
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

fn with_bytes(content_type: &str, bytes: Vec<u8>) -> Response {
    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap_or(HeaderValue::from_static(DEFAULT_CONTENT_TYPE)),
    );
    out.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    (StatusCode::OK, out, bytes).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use barme_engine::Policy;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep();
        AppState {
            engine: Arc::new(Engine::open(path, Policy::default()).unwrap()),
            semantic: None,
        }
    }

    /// State with one owner key, so auth is actually enforced.
    fn state_with_auth() -> (AppState, String) {
        let s = state();
        s.engine
            .create_key(&KeyRecord {
                access_key: "owner".into(),
                secret_key: "secret".into(),
                read_only: false,
                pots: vec![],
                created_at: String::new(),
            })
            .unwrap();
        let basic = format!("Basic {}", STANDARD.encode("owner:secret"));
        (s, basic)
    }

    async fn send(app: Router, req: Request<Body>) -> Response {
        app.oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn open_mode_allows_everything() {
        let st = state();
        st.engine.put("b", "k", b"hi", "text/plain").unwrap();
        let res = send(
            app(st),
            Request::builder().uri("/pots").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn owner_only_endpoint_rejects_anonymous() {
        let (st, _basic) = state_with_auth();
        let res = send(
            app(st),
            Request::builder().uri("/pots").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn owner_credential_is_accepted() {
        let (st, basic) = state_with_auth();
        let res = send(
            app(st),
            Request::builder()
                .uri("/pots")
                .header(header::AUTHORIZATION, basic)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn public_bucket_reads_without_auth() {
        let (st, _basic) = state_with_auth();
        st.engine.put("open", "k", b"hi", "text/plain").unwrap();
        st.engine
            .set_bucket_config(
                "open",
                &BucketConfig {
                    public_read: true,
                    ..Default::default()
                },
            )
            .unwrap();

        let res = send(
            app(st),
            Request::builder()
                .uri("/objects/open/k")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hi");
    }

    #[tokio::test]
    async fn private_bucket_read_denied_without_auth() {
        let (st, _basic) = state_with_auth();
        st.engine.put("secret", "k", b"hi", "text/plain").unwrap();
        let res = send(
            app(st),
            Request::builder()
                .uri("/objects/secret/k")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn owner_can_toggle_visibility() {
        let (st, basic) = state_with_auth();
        st.engine.put("b", "k", b"hi", "text/plain").unwrap();
        let res = send(
            app(st.clone()),
            Request::builder()
                .method("POST")
                .uri("/pots/b/visibility")
                .header(header::AUTHORIZATION, &basic)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"public_read":true}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(st.engine.is_public("b").unwrap());
    }
}
