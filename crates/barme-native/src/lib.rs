//! Native front door. Two jobs: the operations S3 can't express (version
//! history, fetch-by-hash, sync, search), and a browser-friendly app API for a
//! frontend, so a React app can do everything over JSON + Basic auth without
//! signing SigV4 requests.
//!
//! Auth: Basic (access:secret). With no credentials configured the door runs
//! open. Reads obey bucket visibility; writes, deletes, listing, search, and
//! fetch-by-hash require the owner. CORS is permissive so a browser can call in.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use barme_auth::{authorize, Action, Credentials};
use barme_core::{Annotation, BucketConfig, Hash, KeyRecord, Webhook};
use barme_engine::{Engine, EngineError};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::time::Instant;
use tokio_util::io::{StreamReader, SyncIoBridge};
use tower_http::{cors::CorsLayer, trace::TraceLayer};

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub semantic: Option<Arc<barme_semantic::Semantic>>,
    /// Public base URL of the CDN door, e.g. `http://localhost:7375`, used to
    /// build presigned share links.
    pub cdn_base: String,
    /// Largest accepted upload body, in bytes. Enforced by the router.
    pub max_upload_bytes: usize,
    /// Allowed browser CORS origins. `["*"]` (the default) allows any; a specific
    /// list restricts the doors to those origins.
    pub cors_origins: Vec<String>,
    /// When the process came up, for the health uptime reading.
    pub started: Instant,
}

enum NativeError {
    Engine(EngineError),
    Semantic(barme_semantic::SemanticError),
    Forbidden,
    /// The upload's blocking task failed to run (panic or cancellation).
    Internal(String),
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
            NativeError::Engine(e @ EngineError::Locked(..)) => {
                (StatusCode::CONFLICT, e.to_string()).into_response()
            }
            NativeError::Engine(e @ EngineError::InvalidKey(..)) => {
                (StatusCode::BAD_REQUEST, e.to_string()).into_response()
            }
            NativeError::Engine(e @ EngineError::TooLarge { .. }) => {
                (StatusCode::PAYLOAD_TOO_LARGE, e.to_string()).into_response()
            }
            NativeError::Engine(e @ EngineError::Upload(..)) => {
                (StatusCode::BAD_REQUEST, e.to_string()).into_response()
            }
            NativeError::Engine(e) if e.is_bad_input() => {
                (StatusCode::BAD_REQUEST, e.to_string()).into_response()
            }
            NativeError::Engine(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
            NativeError::Semantic(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
            NativeError::Forbidden => {
                (StatusCode::FORBIDDEN, "access denied").into_response()
            }
            NativeError::Internal(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        }
    }
}

/// Build a CORS layer from configured origins. `["*"]` (or any entry of `*`)
/// stays permissive — the open default for local use. A specific list restricts
/// `Access-Control-Allow-Origin` to exactly those origins, so a deployment can
/// stop arbitrary sites from scripting the API from a victim's browser. An entry
/// that isn't a valid origin is dropped; if that empties the list, no
/// cross-origin request is allowed (fail closed).
pub(crate) fn cors_layer(origins: &[String]) -> CorsLayer {
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

pub fn app(state: AppState) -> Router {
    let max_upload = state.max_upload_bytes;
    let cors = cors_layer(&state.cors_origins);
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/stats", get(stats))
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/{access}", delete(delete_key))
        .route("/webhooks", get(list_webhooks).post(create_webhook))
        .route("/webhooks/{id}", delete(delete_webhook))
        .route("/pots", get(list_buckets))
        .route("/pots/{bucket}", delete(delete_bucket))
        .route("/pots/{bucket}/rename", post(rename_bucket))
        .route("/pots/{bucket}/visibility", post(set_visibility))
        .route("/pots/{bucket}/config", get(get_config).put(put_config))
        .route("/pots/{bucket}/objects", get(list_objects))
        .route("/pots/{bucket}/import", post(import))
        .route("/pots/{bucket}/zip", get(zip_objects))
        .route("/ops/copy", post(copy_object))
        .route("/ops/move", post(move_object))
        .route("/objects/{bucket}/{*key}", get(download).put(upload).delete(remove))
        .route("/history/{bucket}/{*key}", get(history))
        .route("/manifest/{bucket}/{*key}", get(manifest))
        // Object sub-resources use a prefix rather than `/objects/.../meta`:
        // axum's catch-all `{*key}` must be the last path segment, so a suffix
        // after it can't be routed. Same prefix style as `/history` and
        // `/manifest` above.
        .route("/meta/{bucket}/{*key}", get(get_meta).put(put_meta))
        .route("/restore/{bucket}/{*key}", post(restore))
        .route("/diff/{bucket}/{*key}", get(diff))
        .route("/verify/{bucket}/{*key}", post(verify))
        .route("/presign/{bucket}/{*key}", post(presign))
        .route("/content/{hash}", get(content))
        .route("/search", post(search))
        .route("/similar/{hash}", post(similar))
        // Merkle: inclusion proofs and the chunk-level delta between versions.
        .route("/proof/{bucket}/{*key}", get(proof))
        .route("/delta/{bucket}/{*key}", get(delta_handler))
        // Sync: replicate an object between stores. Pull = plan then fetch
        // chunks then import; push = put chunks then import.
        .route("/object/{id}", get(object_by_id))
        .route("/chunk/{hash}", get(chunk_get).put(chunk_put))
        .route("/sync/plan", post(sync_plan))
        .route("/sync/import/{bucket}/{*key}", post(import_object_handler))
        .route("/docs", get(docs))
        // Cap the upload body: it's buffered in memory, so an unbounded body
        // could OOM the process. Over the limit gets 413 Payload Too Large.
        .layer(axum::extract::DefaultBodyLimit::max(max_upload))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(state: AppState, listener: tokio::net::TcpListener) -> std::io::Result<()> {
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
        // Constant-time compare: a plain `==` on the secret leaks, through
        // response timing, how many leading bytes a guess got right.
        Some(r) if barme_auth::secret_eq(&r.secret_key, &secret) => Some(r.clone()),
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
    body: Body,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_write(&bucket)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string();
    let object_id = stream_upload(&st, bucket, key, content_type, body).await?;
    Ok(Json(Uploaded {
        object_id: object_id.to_string(),
    })
    .into_response())
}

/// Bridge an async request body into the engine's blocking streaming writer.
/// The body is pulled chunk by chunk on a blocking task, so even a multi-GB
/// upload never fully materializes in memory. The `max_upload_bytes` cap is
/// enforced inside the writer, which returns `TooLarge` -> 413 when exceeded.
async fn stream_upload(
    st: &AppState,
    bucket: String,
    key: String,
    content_type: String,
    body: Body,
) -> Result<Hash, NativeError> {
    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let sync_reader = SyncIoBridge::new(StreamReader::new(stream));
    let engine = st.engine.clone();
    let max = st.max_upload_bytes as u64;
    tokio::task::spawn_blocking(move || {
        engine.put_stream(&bucket, &key, sync_reader, &content_type, max)
    })
    .await
    .map_err(|e| NativeError::Internal(e.to_string()))?
    .map_err(NativeError::from)
}

async fn download(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    let Some((content_type, size, codec, chunks)) = st.engine.object_head(&bucket, &key)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    // Stream the object out one chunk at a time on blocking tasks, so reading a
    // large object never buffers the whole thing in memory. Each chunk self-
    // verifies on read, so integrity holds without a whole-object pass.
    let engine = st.engine.clone();
    let stream = futures_util::stream::iter(chunks).then(move |h| {
        let engine = engine.clone();
        let codec = codec.clone();
        async move {
            tokio::task::spawn_blocking(move || engine.read_chunk(&h, &codec))
                .await
                .map_err(std::io::Error::other)?
                .map(Bytes::from)
                .map_err(std::io::Error::other)
        }
    });

    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, size)
        .body(Body::from_stream(stream))
        .map_err(|e| NativeError::Internal(e.to_string()))
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
    // Stream by-hash delivery the same way as a normal download: one chunk at a
    // time, so fetching a large object by hash never buffers it whole.
    let engine = st.engine.clone();
    let codec = m.storage.codec.clone();
    let stream = futures_util::stream::iter(m.chunking.chunks).then(move |h| {
        let engine = engine.clone();
        let codec = codec.clone();
        async move {
            tokio::task::spawn_blocking(move || engine.read_chunk(&h, &codec))
                .await
                .map_err(std::io::Error::other)?
                .map(Bytes::from)
                .map_err(std::io::Error::other)
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, m.original.content_type)
        .header(header::CONTENT_LENGTH, m.original.size_bytes)
        .body(Body::from_stream(stream))
        .map_err(|e| NativeError::Internal(e.to_string()))
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
    Ok(Json(enrich(&st, &hits)).into_response())
}

/// Objects semantically similar to a stored one. Embeds the object's bytes via
/// the configured proxy and queries the index. Owner-only, like fetch-by-hash.
async fn similar(
    State(st): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let Some(semantic) = &st.semantic else {
        return Ok((StatusCode::NOT_IMPLEMENTED, "semantic search not configured").into_response());
    };
    let Ok(object_id) = hash.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed content hash").into_response());
    };
    let Some(m) = st.engine.object_manifest(&object_id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let bytes = st.engine.read_object(&object_id)?;
    let hits = semantic
        .search(&m.tenant, &bytes, &m.original.content_type, default_k())
        .await?;
    Ok(Json(enrich(&st, &hits)).into_response())
}

/// Turn raw semantic hits into result rows, resolving each id back to its first
/// known pot/key via the reverse index (null when the location is unknown).
fn enrich(st: &AppState, hits: &[barme_semantic::Match]) -> Vec<serde_json::Value> {
    hits.iter()
        .map(|m| {
            let loc = st
                .engine
                .locations(&m.id)
                .ok()
                .and_then(|v| v.into_iter().next());
            serde_json::json!({
                "id": m.id.to_string(),
                "score": m.score,
                "pot": loc.as_ref().map(|(p, _)| p.clone()),
                "key": loc.as_ref().map(|(_, k)| k.clone()),
            })
        })
        .collect()
}

// ---- annotations, versions, integrity -----------------------------------

async fn get_meta(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    Ok(Json(st.engine.annotation(&bucket, &key)?).into_response())
}

async fn put_meta(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(annotation): Json<Annotation>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_write(&bucket)?;
    st.engine.set_annotation(&bucket, &key, &annotation)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct RestoreRequest {
    object_id: String,
}

async fn restore(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<RestoreRequest>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_write(&bucket)?;
    let Ok(object_id) = req.object_id.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed object_id").into_response());
    };
    st.engine.restore_version(&bucket, &key, &object_id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct DiffQuery {
    a: String,
    b: String,
}

async fn diff(
    State(st): State<AppState>,
    Path((bucket, _key)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<DiffQuery>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    let (Ok(a), Ok(b)) = (q.a.parse::<Hash>(), q.b.parse::<Hash>()) else {
        return Ok((StatusCode::BAD_REQUEST, "malformed a or b object_id").into_response());
    };
    Ok(Json(st.engine.diff(&a, &b)?).into_response())
}

async fn verify(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    let ok = st.engine.verify(&bucket, &key)?;
    Ok(Json(serde_json::json!({ "ok": ok })).into_response())
}

#[derive(Deserialize)]
struct PresignRequest {
    expires_secs: u64,
}

async fn presign(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<PresignRequest>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    let Some(secret) = st.engine.signing_secret() else {
        return Ok((
            StatusCode::NOT_IMPLEMENTED,
            "no signing secret (open mode); presigning needs an owner key",
        )
            .into_response());
    };
    let exp = now_unix() + req.expires_secs;
    let sig = barme_auth::presign(&secret, &bucket, &key, exp);
    let url = format!("{}/s/{}/{}?exp={}&sig={}", st.cdn_base, bucket, key, exp, sig);
    Ok(Json(serde_json::json!({ "url": url })).into_response())
}

// ---- import + zip --------------------------------------------------------

#[derive(Deserialize)]
struct ImportRequest {
    url: String,
    key: String,
}

async fn import(
    State(st): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Json(req): Json<ImportRequest>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_write(&bucket)?;
    let resp = match reqwest::get(&req.url).await.and_then(|r| r.error_for_status()) {
        Ok(r) => r,
        Err(e) => return Ok((StatusCode::BAD_GATEWAY, format!("fetch failed: {e}")).into_response()),
    };
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return Ok((StatusCode::BAD_GATEWAY, format!("read failed: {e}")).into_response()),
    };
    let object_id = st.engine.put(&bucket, &req.key, &bytes, &content_type)?;
    Ok(Json(Uploaded {
        object_id: object_id.to_string(),
    })
    .into_response())
}

#[derive(Deserialize)]
struct ZipQuery {
    keys: String,
}

async fn zip_objects(
    State(st): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Query(q): Query<ZipQuery>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default();
        for key in q.keys.split(',').filter(|k| !k.is_empty()) {
            let Some(bytes) = st.engine.get(&bucket, key)? else {
                continue; // skip missing keys rather than fail the whole archive
            };
            if zip.start_file(key, opts).is_err() || zip.write_all(&bytes).is_err() {
                return Ok((StatusCode::INTERNAL_SERVER_ERROR, "zip write failed").into_response());
            }
        }
        if zip.finish().is_err() {
            return Ok((StatusCode::INTERNAL_SERVER_ERROR, "zip finalize failed").into_response());
        }
    }
    let mut out = HeaderMap::new();
    out.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    out.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"barme.zip\""),
    );
    out.insert(header::CONTENT_LENGTH, HeaderValue::from(buf.len()));
    Ok((StatusCode::OK, out, buf).into_response())
}

// ---- webhooks ------------------------------------------------------------

async fn list_webhooks(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    Ok(Json(st.engine.list_webhooks()?).into_response())
}

#[derive(Deserialize)]
struct NewWebhook {
    url: String,
    #[serde(default)]
    events: Vec<String>,
}

async fn create_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(nw): Json<NewWebhook>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    // Id derived from the url and time so it's stable within a call and unique
    // across them, without pulling in a uuid dependency.
    let id = Hash::of(format!("{}:{}", nw.url, now_unix()).as_bytes()).to_hex()[..16].to_string();
    let hook = Webhook {
        id,
        url: nw.url,
        events: nw.events,
    };
    st.engine.add_webhook(&hook)?;
    Ok(Json(hook).into_response())
}

async fn delete_webhook(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    st.engine.delete_webhook(&id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- health + metrics ----------------------------------------------------

/// Liveness and a few headline counts. No auth: safe to expose to a probe.
async fn health(State(st): State<AppState>) -> Result<Response, NativeError> {
    let s = st.engine.stats()?;
    Ok(Json(serde_json::json!({
        "objects": s.objects,
        "pots": s.buckets,
        "unique_chunks": s.unique_chunks,
        "uptime_secs": st.started.elapsed().as_secs(),
    }))
    .into_response())
}

/// Minimal Prometheus text exposition. Formatted by hand; no client dep.
async fn metrics(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let s = st.engine.stats()?;
    let body = format!(
        "# HELP barme_objects Number of live objects.\n\
         # TYPE barme_objects gauge\n\
         barme_objects {}\n\
         # HELP barme_pots Number of pots holding at least one object.\n\
         # TYPE barme_pots gauge\n\
         barme_pots {}\n\
         # HELP barme_unique_chunks Number of unique stored chunks.\n\
         # TYPE barme_unique_chunks gauge\n\
         barme_unique_chunks {}\n\
         # HELP barme_physical_bytes Deduplicated, compressed bytes on disk.\n\
         # TYPE barme_physical_bytes gauge\n\
         barme_physical_bytes {}\n",
        s.objects, s.buckets, s.unique_chunks, s.physical_bytes
    );
    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    Ok((StatusCode::OK, out, body).into_response())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- merkle proofs + sync ------------------------------------------------

#[derive(Deserialize)]
struct ProofQuery {
    index: usize,
}

/// A Merkle inclusion proof for one chunk of a key's current version.
async fn proof(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<ProofQuery>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    match st.engine.prove_chunk(&bucket, &key, q.index)? {
        Some(p) => Ok(Json(p).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// The chunk-level delta between two versions: the exact hashes to transfer.
async fn delta_handler(
    State(st): State<AppState>,
    Path((bucket, _key)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<DiffQuery>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_read(&st, &bucket)?;
    let (Ok(a), Ok(b)) = (q.a.parse::<Hash>(), q.b.parse::<Hash>()) else {
        return Ok((StatusCode::BAD_REQUEST, "malformed a or b object_id").into_response());
    };
    Ok(Json(st.engine.delta(&a, &b)?).into_response())
}

/// Fetch a manifest by object_id, for a puller assembling an object from
/// another store. Owner-only, like fetch-by-hash.
async fn object_by_id(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let Ok(object_id) = id.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed object_id").into_response());
    };
    match st.engine.object_manifest(&object_id)? {
        Some(m) => Ok(Json(m).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// Raw stored bytes of a chunk, to ship it verbatim to another store.
async fn chunk_get(
    State(st): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let Ok(h) = hash.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed chunk hash").into_response());
    };
    match st.engine.chunk_bytes(&h)? {
        Some(bytes) => Ok(with_bytes(DEFAULT_CONTENT_TYPE, bytes)),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// Accept raw chunk bytes from another store. The path hash must match the
/// content, so a corrupt transfer is rejected.
async fn chunk_put(
    State(st): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let Ok(want) = hash.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed chunk hash").into_response());
    };
    let got = st.engine.put_chunk_bytes(&body)?;
    if got != want {
        return Ok((StatusCode::BAD_REQUEST, "chunk hash does not match body").into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct SyncPlanRequest {
    object_id: String,
    #[serde(default)]
    have: Vec<String>,
}

/// Given a target object_id and the chunks the caller already holds, return the
/// manifest and the exact chunks still to fetch. One round trip to plan a pull.
async fn sync_plan(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SyncPlanRequest>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_owner()?;
    let Ok(object_id) = req.object_id.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed object_id").into_response());
    };
    let Some(m) = st.engine.object_manifest(&object_id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let have: std::collections::HashSet<Hash> =
        req.have.iter().filter_map(|s| s.parse::<Hash>().ok()).collect();
    let missing: Vec<String> = m
        .chunking
        .chunks
        .iter()
        .filter(|h| !have.contains(h))
        .map(|h| h.to_string())
        .collect();
    Ok(Json(serde_json::json!({ "manifest": m, "missing": missing })).into_response())
}

/// Adopt a manifest fetched from another store, pointing bucket/key at it. Every
/// chunk it names must already be present (push them first via PUT /chunk).
async fn import_object_handler(
    State(st): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(manifest): Json<barme_core::Manifest>,
) -> Result<Response, NativeError> {
    caller(&st, &headers).require_write(&bucket)?;
    let id = st.engine.import_object(&bucket, &key, &manifest)?;
    Ok(Json(serde_json::json!({ "object_id": id.to_string() })).into_response())
}

/// A simple, self-contained API reference for the native door.
async fn docs() -> axum::response::Html<&'static str> {
    axum::response::Html(DOCS_HTML)
}

const DOCS_HTML: &str = r#"<!doctype html><html><head><meta charset=utf-8>
<title>barme API</title><meta name=viewport content="width=device-width,initial-scale=1">
<style>
body{margin:0;background:#0b0d11;color:#e6e9ef;font:14px/1.6 system-ui,sans-serif}
main{max-width:820px;margin:0 auto;padding:32px 20px}
h1{font-size:1.4rem;letter-spacing:-.02em}h2{margin-top:28px;font-size:.8rem;text-transform:uppercase;letter-spacing:.08em;color:#8b93a1}
.dot{display:inline-block;width:9px;height:9px;border-radius:50%;background:linear-gradient(135deg,#7c6cff,#c084fc);margin-right:8px}
table{width:100%;border-collapse:collapse;margin-top:8px}
td{padding:7px 8px;border-bottom:1px solid #232a33;vertical-align:top}
.m{font:12px ui-monospace,monospace;color:#43d17a;white-space:nowrap}
code{font:12px ui-monospace,monospace;color:#e6e9ef}.d{color:#8b93a1}
a{color:#7c6cff}
.exp{font-size:.6rem;background:#3a2f12;color:#e0b341;padding:2px 6px;border-radius:5px;letter-spacing:.04em;margin-left:8px;vertical-align:middle;text-transform:uppercase}
</style></head><body><main>
<h1><span class=dot></span>barme API</h1>
<p class=d>Native API. Auth is HTTP Basic (access:secret); reads on public pots are open. S3 clients use the S3 door with SigV4.</p>
<p class=d>Sections marked <span class=exp>experimental</span> may change between releases. Everything else is part of the stable v1 API contract. Image codecs (perceptual fidelity) are also experimental — routed and recorded in the manifest, but not yet transcoding.</p>

<h2>Pots</h2><table>
<tr><td class=m>GET</td><td><code>/pots</code></td><td class=d>list pots</td></tr>
<tr><td class=m>DELETE</td><td><code>/pots/{pot}</code></td><td class=d>delete a pot</td></tr>
<tr><td class=m>POST</td><td><code>/pots/{pot}/rename</code></td><td class=d>{new_name}</td></tr>
<tr><td class=m>POST</td><td><code>/pots/{pot}/visibility</code></td><td class=d>{public_read}</td></tr>
<tr><td class=m>GET/PUT</td><td><code>/pots/{pot}/config</code></td><td class=d>storage policy + lifecycle</td></tr>
<tr><td class=m>GET</td><td><code>/pots/{pot}/objects</code></td><td class=d>list objects</td></tr>
<tr><td class=m>POST</td><td><code>/pots/{pot}/import</code></td><td class=d>{url,key} — fetch & store</td></tr>
<tr><td class=m>GET</td><td><code>/pots/{pot}/zip?keys=a,b</code></td><td class=d>download as zip</td></tr>
</table>

<h2>Objects</h2><table>
<tr><td class=m>GET/PUT/DELETE</td><td><code>/objects/{pot}/{key}</code></td><td class=d>download / upload / delete</td></tr>
<tr><td class=m>GET</td><td><code>/manifest/{pot}/{key}</code></td><td class=d>how it was stored</td></tr>
<tr><td class=m>GET</td><td><code>/history/{pot}/{key}</code></td><td class=d>version list</td></tr>
<tr><td class=m>GET/PUT</td><td><code>/meta/{pot}/{key}</code></td><td class=d>tags, note, favorite, lock</td></tr>
<tr><td class=m>POST</td><td><code>/restore/{pot}/{key}</code></td><td class=d>{object_id} — roll back</td></tr>
<tr><td class=m>GET</td><td><code>/diff/{pot}/{key}?a&b</code></td><td class=d>version diff</td></tr>
<tr><td class=m>POST</td><td><code>/verify/{pot}/{key}</code></td><td class=d>re-hash integrity check</td></tr>
<tr><td class=m>POST</td><td><code>/presign/{pot}/{key}</code></td><td class=d>{expires_secs} — share link</td></tr>
<tr><td class=m>GET</td><td><code>/content/{hash}</code></td><td class=d>fetch by content hash</td></tr>
</table>

<h2>Search &amp; AI <span class=exp>experimental</span></h2><table>
<tr><td class=m>POST</td><td><code>/search</code></td><td class=d>{query} — semantic search</td></tr>
<tr><td class=m>POST</td><td><code>/similar/{hash}</code></td><td class=d>nearest objects</td></tr>
</table>

<h2>Merkle &amp; sync <span class=exp>experimental</span></h2><table>
<tr><td class=m>GET</td><td><code>/proof/{pot}/{key}?index</code></td><td class=d>inclusion proof for one chunk</td></tr>
<tr><td class=m>GET</td><td><code>/delta/{pot}/{key}?a&b</code></td><td class=d>chunks to transfer between versions</td></tr>
<tr><td class=m>GET</td><td><code>/object/{id}</code></td><td class=d>manifest by object_id</td></tr>
<tr><td class=m>GET/PUT</td><td><code>/chunk/{hash}</code></td><td class=d>raw chunk bytes (ship / receive)</td></tr>
<tr><td class=m>POST</td><td><code>/sync/plan</code></td><td class=d>{object_id, have[]} — manifest + missing chunks</td></tr>
<tr><td class=m>POST</td><td><code>/sync/import/{pot}/{key}</code></td><td class=d>adopt a fetched manifest</td></tr>
</table>

<h2>Admin</h2><table>
<tr><td class=m>GET</td><td><code>/stats</code> · <code>/health</code> · <code>/metrics</code></td><td class=d>storage stats, health, Prometheus</td></tr>
<tr><td class=m>GET/POST</td><td><code>/keys</code></td><td class=d>list / create access keys</td></tr>
<tr><td class=m>DELETE</td><td><code>/keys/{access}</code></td><td class=d>revoke a key</td></tr>
<tr><td class=m>GET/POST</td><td><code>/webhooks</code> <span class=exp>experimental</span></td><td class=d>list / create webhooks</td></tr>
</table>
</main></body></html>"#;

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
            cdn_base: "http://localhost:7375".into(),
            max_upload_bytes: 512 * 1024 * 1024,
            cors_origins: vec!["*".into()],
            started: Instant::now(),
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
    async fn cors_restricts_to_configured_origins() {
        // A restricted list echoes only the allowed origin back; an unlisted one
        // gets no Access-Control-Allow-Origin, so a browser blocks the read. This
        // is what makes the cors_origins config knob actually do something.
        let mut st = state();
        st.cors_origins = vec!["https://good.example".into()];

        let allowed = send(
            app(st.clone()),
            Request::builder()
                .uri("/pots")
                .header("origin", "https://good.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            allowed
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://good.example"),
        );

        let blocked = send(
            app(st),
            Request::builder()
                .uri("/pots")
                .header("origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(
            blocked
                .headers()
                .get("access-control-allow-origin")
                .is_none(),
            "an unlisted origin must not be granted CORS access",
        );
    }

    #[tokio::test]
    async fn cors_star_is_permissive() {
        // The default ["*"] allows any origin — the open, local-friendly default.
        let res = send(
            app(state()),
            Request::builder()
                .uri("/pots")
                .header("origin", "https://anything.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(
            res.headers().get("access-control-allow-origin").is_some(),
            "permissive CORS should allow any origin",
        );
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

    // Multi-thread runtime: the streaming upload bridges the async body into a
    // blocking chunker via SyncIoBridge, which needs another runtime thread to
    // drive the body while the blocking task reads it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_upload_is_rejected_with_413() {
        let mut st = state(); // open mode
        st.max_upload_bytes = 16; // tiny cap for the test
        let res = send(
            app(st),
            Request::builder()
                .method("PUT")
                .uri("/objects/b/big")
                .body(Body::from(vec![0u8; 1024])) // 1 KiB over a 16 B cap
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_upload_and_download_round_trip() {
        let st = state(); // open mode
        // A few MB of varied bytes: many chunks, both write and read exercise
        // the streaming paths (upload via SyncIoBridge, download via the
        // chunk-at-a-time body stream).
        let body: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        let put = send(
            app(st.clone()),
            Request::builder()
                .method("PUT")
                .uri("/objects/b/big.bin")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(put.status(), StatusCode::OK);

        let get = send(
            app(st),
            Request::builder()
                .uri("/objects/b/big.bin")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(
            get.headers().get(header::CONTENT_LENGTH).unwrap(),
            &body.len().to_string(),
            "streamed download must report the true content length"
        );
        let got = get.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(got.len(), body.len(), "streamed download length");
        assert_eq!(&got[..], &body[..], "streamed body must read back intact");
    }

    #[tokio::test]
    async fn overlong_key_is_rejected_with_400() {
        let st = state(); // open mode
        let key = "a".repeat(200); // over MAX_KEY_LEN
        let res = send(
            app(st),
            Request::builder()
                .method("PUT")
                .uri(format!("/objects/b/{key}"))
                .body(Body::from("x"))
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
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
