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
            p.crumb { a href="/" { "← all buckets" } }
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
            p.crumb { a href=(format!("/b/{}", enc(&bucket))) { "← " (bucket) } }
            h2 { (key) }

            div.badges {
                span.badge { (format!("{:?}", m.storage.route).to_lowercase()) }
                span class={ "badge badge--" (format!("{:?}", m.storage.fidelity).to_lowercase()) } {
                    (format!("{:?}", m.storage.fidelity).to_lowercase())
                }
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
            p.crumb { a href="/" { "← home" } }
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
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>🍲</text></svg>";
                style { (PreEscaped(CSS)) }
            }
            body {
                header {
                    div.bar {
                        a.brand href="/" {
                            span.dot {}
                            "barme"
                        }
                        span.tag { "object store" }
                    }
                }
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
                div.search-row {
                    input name="q" value=(q) placeholder="search by meaning";
                    button { "search" }
                }
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
:root {
  color-scheme: light dark;
  --bg: #f6f7f9;
  --surface: #ffffff;
  --text: #1a1d24;
  --muted: #6b7280;
  --border: #e5e7eb;
  --accent: #6d5efc;
  --accent-ink: #ffffff;
  --ok: #0f9d58;
  --ok-bg: #e6f4ea;
  --warn: #b26a00;
  --warn-bg: #fdf0d5;
  --danger: #dc2626;
  --shadow: 0 1px 2px rgba(16,18,27,.06), 0 4px 16px rgba(16,18,27,.05);
  --radius: 12px;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d0f14;
    --surface: #161922;
    --text: #e7e9ee;
    --muted: #8b93a5;
    --border: #262b36;
    --accent: #8b7dff;
    --ok: #4ade80; --ok-bg: #10331f;
    --warn: #fbbf24; --warn-bg: #3a2c08;
    --danger: #f87171;
    --shadow: none;
  }
}
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body {
  margin: 0;
  font: 15px/1.6 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  background: var(--bg);
  color: var(--text);
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }

header {
  position: sticky; top: 0; z-index: 5;
  background: color-mix(in srgb, var(--surface) 88%, transparent);
  backdrop-filter: saturate(1.6) blur(8px);
  border-bottom: 1px solid var(--border);
}
.bar {
  max-width: 940px; margin: 0 auto; padding: 14px 22px;
  display: flex; align-items: baseline; gap: 12px;
}
.brand {
  color: var(--text); font-weight: 700; font-size: 1.15rem;
  letter-spacing: -.02em; display: inline-flex; align-items: center; gap: 8px;
}
.brand:hover { text-decoration: none; }
.dot {
  width: 10px; height: 10px; border-radius: 50%;
  background: linear-gradient(135deg, var(--accent), #c084fc);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 20%, transparent);
}
.tag { color: var(--muted); font-size: .82rem; }

main { max-width: 940px; margin: 0 auto; padding: 28px 22px 60px; }

section {
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  padding: 20px 22px;
  margin: 0 0 20px;
}
h2 { font-size: .95rem; text-transform: uppercase; letter-spacing: .06em; color: var(--muted); margin: 0 0 14px; font-weight: 600; }
h3 { font-size: 1rem; margin: 24px 0 10px; }

table { width: 100%; border-collapse: collapse; }
th, td { text-align: left; padding: 10px 12px; border-bottom: 1px solid var(--border); vertical-align: middle; }
th { font-size: .78rem; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); font-weight: 600; }
tbody tr:last-child td, tr:last-child td { border-bottom: none; }
table a { font-weight: 500; }
td:not(:first-child), th:not(:first-child) { color: var(--muted); }

code {
  font-family: ui-monospace, "SF Mono", Menlo, monospace;
  font-size: .82em; word-break: break-all;
  background: color-mix(in srgb, var(--text) 7%, transparent);
  padding: 1px 5px; border-radius: 5px;
}

.muted { color: var(--muted); }

label { display: block; margin: 0 0 12px; font-size: .88rem; color: var(--muted); }
input {
  display: block; width: 100%; margin-top: 5px;
  padding: 9px 11px; font: inherit;
  background: var(--bg); color: var(--text);
  border: 1px solid var(--border); border-radius: 8px;
}
input[type=file] { padding: 7px; }
input:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 22%, transparent); }

button {
  padding: 9px 18px; font: inherit; font-weight: 600; cursor: pointer;
  background: var(--accent); color: var(--accent-ink);
  border: none; border-radius: 8px; margin-top: 4px;
}
button:hover { filter: brightness(1.06); }
button.danger { background: transparent; color: var(--danger); border: 1px solid color-mix(in srgb, var(--danger) 40%, var(--border)); }
button.danger:hover { background: color-mix(in srgb, var(--danger) 12%, transparent); filter: none; }
form.inline { display: inline; }

.search-row { display: flex; gap: 8px; }
.search-row input { margin-top: 0; }
.search-row button { margin-top: 0; white-space: nowrap; }

.badges { display: flex; flex-wrap: wrap; gap: 8px; margin: 4px 0 18px; }
.badge {
  display: inline-flex; align-items: center;
  padding: 3px 10px; border-radius: 999px; font-size: .78rem; font-weight: 600;
  background: color-mix(in srgb, var(--text) 7%, transparent); color: var(--text);
}
.badge--exact { background: var(--ok-bg); color: var(--ok); }
.badge--perceptual { background: var(--warn-bg); color: var(--warn); }

ol.versions { padding-left: 20px; }
.versions li { margin: 6px 0; }

.crumb { margin: -4px 0 18px; font-size: .9rem; }
"#;
