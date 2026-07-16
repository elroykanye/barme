//! Embedded web console. Server-rendered HTML with plain forms, no JavaScript
//! and no external assets, so it ships in the one binary and works offline.
//! It reads and writes through the engine directly, same as the doors.
//!
//! Pages: a dashboard, a per-bucket key list, a per-object view with manifest
//! and version history, upload/download/delete, and semantic search when it's
//! configured.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use barme_core::Hash;
use barme_engine::{Engine, EngineError};
use barme_semantic::{Semantic, SemanticError};
use maud::{html, Markup, PreEscaped, DOCTYPE};

#[derive(Clone)]
pub struct ConsoleState {
    pub engine: Arc<Engine>,
    pub semantic: Option<Arc<Semantic>>,
}

enum ConsoleError {
    Engine(EngineError),
    Semantic(SemanticError),
    Bad(String),
}

impl From<EngineError> for ConsoleError {
    fn from(e: EngineError) -> Self {
        ConsoleError::Engine(e)
    }
}
impl From<SemanticError> for ConsoleError {
    fn from(e: SemanticError) -> Self {
        ConsoleError::Semantic(e)
    }
}

impl IntoResponse for ConsoleError {
    fn into_response(self) -> Response {
        match self {
            ConsoleError::Engine(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
            ConsoleError::Semantic(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
            ConsoleError::Bad(m) => (StatusCode::BAD_REQUEST, m).into_response(),
        }
    }
}

pub fn app(state: ConsoleState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/b/{bucket}", get(bucket_view))
        .route("/o/{bucket}/{*key}", get(object_view))
        .route("/download/{bucket}/{*key}", get(download))
        .route("/c/{hash}", get(download_by_hash))
        .route("/upload", post(upload))
        .route("/delete/{bucket}/{*key}", post(delete_object))
        .route("/search", get(search))
        .with_state(state)
}

pub async fn serve(state: ConsoleState, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app(state)).await
}

// ---- pages ---------------------------------------------------------------

async fn dashboard(State(st): State<ConsoleState>) -> Result<Html<String>, ConsoleError> {
    let buckets = st.engine.buckets()?;
    let mut rows = Vec::new();
    for b in &buckets {
        rows.push((b.clone(), st.engine.keys(b)?.len()));
    }

    Ok(page(
        "barme",
        html! {
            section {
                h2 { "buckets" }
                @if rows.is_empty() {
                    p.muted { "nothing stored yet. upload something below." }
                } @else {
                    table {
                        tr { th { "bucket" } th { "objects" } }
                        @for (b, n) in &rows {
                            tr {
                                td { a href=(format!("/b/{}", enc(b))) { (b) } }
                                td { (n) }
                            }
                        }
                    }
                }
            }
            section {
                h2 { "upload" }
                form method="post" action="/upload" enctype="multipart/form-data" {
                    label { "bucket " input name="bucket" required; }
                    label { "key (optional, defaults to filename) " input name="key"; }
                    input type="file" name="file" required;
                    button { "upload" }
                }
            }
            (search_form(&st, ""))
        },
    ))
}

async fn bucket_view(
    State(st): State<ConsoleState>,
    Path(bucket): Path<String>,
) -> Result<Html<String>, ConsoleError> {
    let keys = st.engine.keys(&bucket)?;
    let mut rows = Vec::new();
    for k in &keys {
        let (size, versions) = match st.engine.manifest(&bucket, k)? {
            Some(m) => (m.original.size_bytes, st.engine.history(&bucket, k)?.len()),
            None => (0, 0),
        };
        rows.push((k.clone(), size, versions));
    }

    Ok(page(
        &format!("barme / {bucket}"),
        html! {
            p { a href="/" { "← all buckets" } }
            h2 { (bucket) }
            @if rows.is_empty() {
                p.muted { "empty bucket." }
            } @else {
                table {
                    tr { th { "key" } th { "size" } th { "versions" } th {} }
                    @for (k, size, versions) in &rows {
                        tr {
                            td { a href=(format!("/o/{}/{}", enc(&bucket), enc(k))) { (k) } }
                            td { (human(*size)) }
                            td { (versions) }
                            td {
                                a href=(format!("/download/{}/{}", enc(&bucket), enc(k))) { "download" }
                            }
                        }
                    }
                }
            }
        },
    ))
}

async fn object_view(
    State(st): State<ConsoleState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, ConsoleError> {
    let Some(m) = st.engine.manifest(&bucket, &key)? else {
        return Ok((StatusCode::NOT_FOUND, "no such object").into_response());
    };
    let history = st.engine.history(&bucket, &key)?;

    let ratio = if m.original.size_bytes > 0 {
        100.0 * m.storage.stored_size_bytes as f64 / m.original.size_bytes as f64
    } else {
        100.0
    };

    Ok(page(
        &format!("barme / {bucket} / {key}"),
        html! {
            p { a href=(format!("/b/{}", enc(&bucket))) { "← " (bucket) } }
            h2 { (key) }

            div.badges {
                span.badge { (format!("{:?}", m.storage.route).to_lowercase()) }
                span.badge { (format!("{:?}", m.storage.fidelity).to_lowercase()) }
                span.badge { "codec: " (m.storage.codec) }
                @if let Some(score) = m.quality.score {
                    span.badge { "quality: " (format!("{score:.3}")) }
                }
            }

            table {
                tr { th { "original" } td { (human(m.original.size_bytes)) } }
                tr { th { "stored" } td { (human(m.storage.stored_size_bytes)) " (" (format!("{ratio:.0}")) "% of original)" } }
                tr { th { "content-type" } td { (m.original.content_type) } }
                tr { th { "chunks" } td { (m.chunking.chunks.len()) } }
                tr { th { "object id" } td { code { (m.object_id.to_string()) } } }
            }

            p {
                a href=(format!("/download/{}/{}", enc(&bucket), enc(&key))) { "download current" }
                " · "
                form.inline method="post" action=(format!("/delete/{}/{}", enc(&bucket), enc(&key))) {
                    button.danger { "delete" }
                }
            }

            h3 { "versions (" (history.len()) ")" }
            ol.versions {
                @for id in &history {
                    li {
                        code { (id.to_string()) }
                        " "
                        a href=(format!("/c/{}", enc(&id.to_string()))) { "fetch" }
                    }
                }
            }
        },
    )
    .into_response())
}

async fn search(
    State(st): State<ConsoleState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Html<String>, ConsoleError> {
    let q = params.get("q").cloned().unwrap_or_default();

    let unconfigured = st.semantic.is_none() && !q.is_empty();
    let results = if q.is_empty() {
        None
    } else if let Some(sem) = &st.semantic {
        Some(sem.search("default", q.as_bytes(), "text/plain", 20).await?)
    } else {
        None
    };

    Ok(page(
        "barme / search",
        html! {
            p { a href="/" { "← home" } }
            (search_form(&st, &q))
            @if unconfigured {
                p.muted { "semantic search isn't configured (set BARME_EMBED_URL on the server)." }
            }
            @if let Some(hits) = &results {
                h3 { (hits.len()) " results" }
                ol {
                    @for hit in hits {
                        li {
                            code { (hit.id.to_string()) }
                            " · score " (format!("{:.3}", hit.score))
                            " · " a href=(format!("/c/{}", enc(&hit.id.to_string()))) { "fetch" }
                        }
                    }
                }
            }
        },
    ))
}

// ---- actions -------------------------------------------------------------

async fn upload(
    State(st): State<ConsoleState>,
    mut form: Multipart,
) -> Result<Redirect, ConsoleError> {
    let mut bucket = None;
    let mut key = None;
    let mut filename = None;
    let mut content_type = "application/octet-stream".to_string();
    let mut data = None;

    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| ConsoleError::Bad(e.to_string()))?
    {
        let name = field.name().map(str::to_string);
        match name.as_deref() {
            Some("bucket") => bucket = Some(text(field).await?),
            Some("key") => key = Some(text(field).await?),
            Some("file") => {
                filename = field.file_name().map(str::to_string);
                if let Some(ct) = field.content_type() {
                    content_type = ct.to_string();
                }
                data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| ConsoleError::Bad(e.to_string()))?
                        .to_vec(),
                );
            }
            _ => {}
        }
    }

    let bucket = bucket
        .filter(|b| !b.is_empty())
        .ok_or_else(|| ConsoleError::Bad("bucket is required".into()))?;
    let key = key
        .filter(|k| !k.is_empty())
        .or(filename)
        .ok_or_else(|| ConsoleError::Bad("key or a named file is required".into()))?;
    let data = data.ok_or_else(|| ConsoleError::Bad("no file".into()))?;

    st.engine.put(&bucket, &key, &data, &content_type)?;
    Ok(Redirect::to(&format!("/b/{}", enc(&bucket))))
}

async fn delete_object(
    State(st): State<ConsoleState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Redirect, ConsoleError> {
    st.engine.delete(&bucket, &key)?;
    Ok(Redirect::to(&format!("/b/{}", enc(&bucket))))
}

async fn download(
    State(st): State<ConsoleState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, ConsoleError> {
    let Some(bytes) = st.engine.get(&bucket, &key)? else {
        return Ok((StatusCode::NOT_FOUND, "no such object").into_response());
    };
    let content_type = st
        .engine
        .manifest(&bucket, &key)?
        .map(|m| m.original.content_type)
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let filename = key.rsplit('/').next().unwrap_or("download");
    Ok(attachment(&content_type, filename, bytes))
}

async fn download_by_hash(
    State(st): State<ConsoleState>,
    Path(hash): Path<String>,
) -> Result<Response, ConsoleError> {
    let Ok(id) = hash.parse::<Hash>() else {
        return Ok((StatusCode::BAD_REQUEST, "malformed hash").into_response());
    };
    let Some(m) = st.engine.object_manifest(&id)? else {
        return Ok((StatusCode::NOT_FOUND, "no such object").into_response());
    };
    let bytes = st.engine.read_object(&id)?;
    Ok(attachment(&m.original.content_type, &id.to_hex(), bytes))
}

// ---- shared bits ---------------------------------------------------------

fn page(title: &str, body: Markup) -> Html<String> {
    let markup = html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(CSS)) }
            }
            body {
                header { a.brand href="/" { "barme" } }
                main { (body) }
            }
        }
    };
    Html(markup.into_string())
}

fn search_form(st: &ConsoleState, q: &str) -> Markup {
    html! {
        section {
            h2 { "search" }
            @if st.semantic.is_none() {
                p.muted { "not configured; set BARME_EMBED_URL on the server to enable." }
            }
            form method="get" action="/search" {
                input name="q" value=(q) placeholder="search by meaning";
                button { "search" }
            }
        }
    }
}

fn attachment(content_type: &str, filename: &str, bytes: Vec<u8>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    (StatusCode::OK, headers, bytes).into_response()
}

async fn text(field: axum::extract::multipart::Field<'_>) -> Result<String, ConsoleError> {
    field.text().await.map_err(|e| ConsoleError::Bad(e.to_string()))
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < UNITS.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    format!("{f:.1} {}", UNITS[i])
}

const CSS: &str = r#"
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body { font: 15px/1.5 system-ui, sans-serif; margin: 0; }
header { padding: 12px 20px; border-bottom: 1px solid #8883; }
.brand { font-weight: 700; letter-spacing: .5px; text-decoration: none; color: inherit; }
main { max-width: 900px; margin: 0 auto; padding: 20px; }
section { margin: 0 0 28px; }
h2 { font-size: 1.1rem; margin: 0 0 10px; }
table { width: 100%; border-collapse: collapse; }
th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid #8882; vertical-align: top; }
th { font-weight: 600; }
code { font-family: ui-monospace, monospace; font-size: .85em; word-break: break-all; }
a { color: #3b82f6; }
.muted { color: #8889; }
label { display: block; margin: 6px 0; }
input { padding: 6px 8px; font: inherit; }
button { padding: 6px 14px; font: inherit; cursor: pointer; margin-top: 8px; }
button.danger { color: #ef4444; }
form.inline { display: inline; }
.badges { margin: 8px 0 16px; }
.badge { display: inline-block; padding: 2px 8px; margin-right: 6px; border: 1px solid #8884; border-radius: 4px; font-size: .8rem; }
.versions li { margin: 4px 0; }
"#;
