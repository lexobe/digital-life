//! Creator-facing previews of registered services. An operator can backfill a site
//! under agents/<id>/previews/<port>/; organisms can publish multi-file offline
//! sites under /work/public/<port>/. Neither route starts an organism.
use super::{ApiError, App, open_regular};
use axum::{
    Json,
    body::Body,
    http::{StatusCode, response::Builder},
    response::Response,
};
use cap_std::fs::Dir;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    fs, io,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use survive_core::{Agent, AgentState, ServiceRegistration};

const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CAPTURED_PORTS: usize = 8;
const MAX_CAPTURE_FILES: usize = 24;
const MAX_CAPTURE_TOTAL: usize = 8 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 16 * 1024 * 1024;

pub(super) fn endpoint(app: &App, agent: &Agent, port: u16) -> Result<String, ApiError> {
    if let Some(sandbox) = &app.0.sandbox {
        Ok(sandbox.service_endpoint(&agent.id, port)?)
    } else {
        Ok(format!("http://{}:{port}/", agent.source_ip))
    }
}

pub(super) async fn service_list(
    app: &App,
    agent: Agent,
    registrations: Vec<ServiceRegistration>,
) -> Result<Json<Value>, ApiError> {
    let agent_dir = app.0.config.data_dir.join("agents").join(&agent.id);
    let services = tokio::task::spawn_blocking(move || {
        registrations.into_iter().map(|service| {
            let archive = Archive::find(&agent_dir, service.port);
            let entry = archive.as_ref().map(|_| "index.html");
            let detail = archive.as_ref().map(Archive::detail);
            let mode = if agent.state == AgentState::Running {
                "live"
            } else if entry.is_some() {
                "archive"
            } else {
                "unavailable"
            };
            json!({"port": service.port, "name": service.name, "preview": {"mode": mode, "entry": entry, "detail": detail}})
        }).collect::<Vec<_>>()
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({"services": services})))
}

pub(super) fn response_headers(response: Builder, mode: &'static str) -> Builder {
    response
        .header("x-survive-preview", mode)
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "no-referrer")
        .header(
            "content-security-policy",
            "sandbox allow-scripts allow-forms; default-src 'self' data: blob:; connect-src 'self'",
        )
}

// Open one directory component at a time, pinning its descriptor before
// descending. In particular /work/public/<port> must never follow a guest
// symlink into another part of the workspace.
fn child_dir(parent: &Dir, name: &str) -> Option<Dir> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, openat};
        let fd = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
        Some(Dir::from_std_file(fd.into()))
    }
    #[cfg(not(unix))]
    {
        if parent.symlink_metadata(name).ok()?.file_type().is_symlink() {
            return None;
        }
        parent.open_dir(name).ok()
    }
}

enum Archive {
    Site(Dir),
    Capture {
        tar: tar::Archive<fs::File>,
        partial: bool,
    },
}

fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains(['\\', '%'])
        && path.split('/').all(|part| {
            !part.is_empty() && !part.starts_with('.') && !part.chars().any(char::is_control)
        })
}

impl Archive {
    fn site(parent: &Dir, directory: &str, port: u16) -> Option<Self> {
        let root = child_dir(&child_dir(parent, directory)?, &port.to_string())?;
        let entry = open_regular(&root, "index.html").ok()?;
        (entry.metadata().ok()?.len() <= MAX_ASSET_BYTES).then_some(Self::Site(root))
    }

    fn find(agent_dir: &Path, port: u16) -> Option<Self> {
        let dir = Dir::open_ambient_dir(agent_dir, cap_std::ambient_authority()).ok()?;
        Self::site(&dir, "previews", port)
            .or_else(|| Self::site(&child_dir(&dir, "work")?, "public", port))
            .or_else(|| {
                let file = open_regular(
                    &child_dir(&dir, "preview-captures")?,
                    &format!("{port}.tar"),
                )
                .ok()?;
                let mut tar = tar::Archive::new(file.into_std());
                let partial = {
                    let mut entries = tar.entries_with_seek().ok()?;
                    let marker = entries.next()?.ok()?;
                    match marker.path_bytes().as_ref() {
                        b".partial" => true,
                        b".captured" => false,
                        _ => return None,
                    }
                };
                let mut file = tar.into_inner();
                file.seek(SeekFrom::Start(0)).ok()?;
                Some(Self::Capture {
                    tar: tar::Archive::new(file),
                    partial,
                })
            })
    }

    fn detail(&self) -> &'static str {
        match self {
            Self::Site(_) => "static-site",
            Self::Capture { partial: true, .. } => "partial-capture",
            Self::Capture { partial: false, .. } => "http-capture",
        }
    }

    fn response(self, path: &str, head: bool) -> Result<Response, ApiError> {
        let missing = || ApiError(StatusCode::NOT_FOUND, "Preview asset not found".into());
        let path = if path.is_empty() { "index.html" } else { path };
        if !safe_path(path) {
            return Err(missing());
        }
        let detail = self.detail();
        match self {
            Self::Site(mut directory) => {
                let mut components = path.split('/').peekable();
                let name = loop {
                    let component = components.next().ok_or_else(missing)?;
                    if components.peek().is_none() {
                        break component;
                    }
                    directory = child_dir(&directory, component).ok_or_else(missing)?;
                };
                let file = open_regular(&directory, name).map_err(|_| missing())?;
                let len = file.metadata().map_err(|_| missing())?.len();
                asset_response(file, len, path, head, detail)
            }
            Self::Capture { mut tar, .. } => {
                for entry in tar.entries_with_seek().map_err(|_| missing())? {
                    let entry = entry.map_err(|_| missing())?;
                    if entry.path_bytes().as_ref() == path.as_bytes() {
                        let len = entry.size();
                        return asset_response(entry, len, path, head, detail);
                    }
                }
                Err(missing())
            }
        }
    }
}

fn asset_response(
    file: impl Read,
    len: u64,
    name: &str,
    head: bool,
    detail: &str,
) -> Result<Response, ApiError> {
    let too_large = || {
        ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Preview asset exceeds 16 MiB".into(),
        )
    };
    if len > MAX_ASSET_BYTES {
        return Err(too_large());
    }
    let mut response = response_headers(Response::builder(), "archive")
        .header("x-survive-preview-detail", detail)
        .header("content-type", mime(name));
    let body = if head {
        response = response.header("content-length", len);
        Body::empty()
    } else {
        let mut bytes = Vec::with_capacity(len as usize);
        file.take(MAX_ASSET_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ApiError::internal("Preview asset read failed"))?;
        if bytes.len() as u64 > MAX_ASSET_BYTES {
            return Err(too_large());
        }
        Body::from(bytes)
    };
    response
        .body(body)
        .map_err(|e| ApiError::internal(e.to_string()))
}

fn mime(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "csv" => "text/csv; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        "zip" => "application/zip",
        "pdf" => "application/pdf",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

pub(super) async fn serve_archive(
    agent_dir: PathBuf,
    port: u16,
    path: String,
    head: bool,
) -> Result<Response, ApiError> {
    tokio::task::spawn_blocking(move || {
        let archive = Archive::find(&agent_dir, port).ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "The service is offline and has no saved HTML preview".into(),
            )
        })?;
        archive.response(&path, head)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
}

pub(super) fn capturable(response: &reqwest::Response) -> bool {
    response.status() == StatusCode::OK
        && response
            .content_length()
            .is_none_or(|n| n <= MAX_CAPTURE_BYTES as u64)
        && response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.split(';').next())
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("text/html"))
}

struct CapturedSite {
    files: Vec<(String, Vec<u8>)>,
    partial: bool,
}

fn store_capture(dir: &Path, port: u16, site: CapturedSite) -> io::Result<()> {
    static WRITER: Mutex<()> = Mutex::new(());
    let _writer = WRITER
        .lock()
        .map_err(|_| io::Error::other("preview writer unavailable"))?;
    let captures = dir.join("preview-captures");
    fs::create_dir_all(&captures)?;
    let target = captures.join(format!("{port}.tar"));
    if !target.exists()
        && fs::read_dir(&captures)?.take(MAX_CAPTURED_PORTS).count() >= MAX_CAPTURED_PORTS
    {
        return Ok(());
    }
    // Rename a whole snapshot. Readers holding the previous descriptor see
    // a coherent set of files even across concurrent re-registration.
    let mut temporary = tempfile::NamedTempFile::new_in(&captures)?;
    let mut tar = tar::Builder::new(temporary.as_file_mut());
    let marker = if site.partial {
        ".partial"
    } else {
        ".captured"
    };
    for (name, bytes) in std::iter::once((marker.to_string(), Vec::new())).chain(site.files) {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o444);
        tar.append_data(&mut header, name, bytes.as_slice())?;
    }
    tar.into_inner()?;
    temporary.persist(target)?;
    Ok(())
}

fn linked_files(html: &[u8], base: &url::Url) -> (Vec<(String, url::Url)>, bool) {
    let Ok(text) = std::str::from_utf8(html) else {
        return (Vec::new(), true);
    };
    let Ok(dom) = tl::parse(text, tl::ParserOptions::default()) else {
        return (Vec::new(), true);
    };
    let mut references = Vec::new();
    let mut partial = false;
    for tag in dom.nodes().iter().filter_map(tl::Node::as_tag) {
        let attrs = tag.attributes();
        let name = tag.name().as_utf8_str();
        let attr = match name.to_ascii_lowercase().as_str() {
            "img" | "script" => "src",
            "a" | "link" => "href",
            "base" => {
                partial |= attrs.contains("href");
                continue;
            }
            _ => continue,
        };
        let Some(raw) = attrs.get(attr).flatten() else {
            continue;
        };
        let source = raw.as_utf8_str();
        let raw = source.trim();
        if raw.is_empty() || raw.starts_with('#') || raw.starts_with("data:") {
            continue;
        }
        if name.eq_ignore_ascii_case("a") && !attrs.contains("download") {
            let name = raw
                .split(['?', '#'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ![
                ".zip", ".gz", ".tgz", ".tar", ".txt", ".csv", ".tsv", ".md", ".json", ".pdf",
                ".xlsx", ".xls", ".py",
            ]
            .iter()
            .any(|ext| name.ends_with(ext))
            {
                continue;
            }
        }
        // Registering a service authorizes fetching resources only from the
        // registered namespace IP/port. Ignore <base>, redirects and absolute URLs.
        let raw_path = raw.split(['?', '#']).next().unwrap_or_default();
        let relative = raw_path.strip_prefix('/').unwrap_or(raw_path);
        let normalized = relative.strip_prefix("./").unwrap_or(relative);
        let Ok(mut url) = base.join(raw) else {
            partial = true;
            continue;
        };
        let path = url.path().trim_start_matches('/').to_string();
        if raw.starts_with("//")
            || raw_path.contains(':')
            || !safe_path(normalized)
            || !safe_path(&path)
            || url.origin() != base.origin()
            || path == "index.html"
        {
            partial = true;
            continue;
        }
        url.set_fragment(None);
        if let Some((_, previous)) = references.iter().find(|(p, _)| *p == path) {
            partial |= *previous != url;
            continue;
        }
        if references.len() >= MAX_CAPTURE_FILES {
            partial = true;
            break;
        }
        references.push((path, url));
    }
    (references, partial)
}

async fn bounded_asset(
    client: &reqwest::Client,
    url: url::Url,
    limit: usize,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let response = client.get(url).timeout(timeout).send().await.ok()?;
    if response.status() != StatusCode::OK
        || response.content_length().is_some_and(|n| n > limit as u64)
    {
        return None;
    }
    let body = Body::from_stream(response.bytes_stream());
    Some(axum::body::to_bytes(body, limit).await.ok()?.to_vec())
}

#[derive(Clone)]
pub(super) struct Capture {
    client: reqwest::Client,
    base: url::Url,
    dir: PathBuf,
    port: u16,
}

impl Capture {
    pub(super) fn new(client: &reqwest::Client, base: url::Url, dir: PathBuf, port: u16) -> Self {
        Self {
            client: client.clone(),
            base,
            dir,
            port,
        }
    }

    async fn save(self, html: Vec<u8>) {
        let base = self.base;
        let Ok((references, mut partial, html)) = tokio::task::spawn_blocking(move || {
            let (refs, partial) = linked_files(&html, &base);
            (refs, partial, html)
        })
        .await
        else {
            return;
        };
        let mut budget = MAX_CAPTURE_TOTAL - html.len();
        let mut files = vec![("index.html".to_string(), html)];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        for (name, url) in references {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || budget == 0 {
                partial = true;
                break;
            }
            if let Some(bytes) = bounded_asset(&self.client, url, budget, remaining).await {
                // Nested CSS resources need an explicitly published multi-file
                // site if they were not present among the HTML's references.
                if name.ends_with(".css") {
                    let css = String::from_utf8_lossy(&bytes);
                    partial |= css.contains("url(") || css.contains("@import");
                }
                budget -= bytes.len();
                files.push((name, bytes));
            } else {
                partial = true;
            }
        }
        let site = CapturedSite { files, partial };
        let port = self.port;
        let result =
            tokio::task::spawn_blocking(move || store_capture(&self.dir, port, site)).await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(port, "could not save service preview");
        }
    }
}

// Mirror only complete, bounded HTML as it streams. Oversize roots, upstream
// errors, and client cancellation never replace a previously saved capture.
pub(super) fn capture_body(response: reqwest::Response, capture: Capture) -> Body {
    let state = (response.bytes_stream(), Some(Vec::new()), capture);
    Body::from_stream(stream::unfold(
        state,
        |(mut upstream, mut html, capture)| async move {
            let next = upstream.next().await;
            match &next {
                Some(Ok(bytes)) => {
                    if let Some(buf) = html.as_mut() {
                        if bytes.len() <= MAX_CAPTURE_BYTES - buf.len() {
                            buf.extend_from_slice(bytes);
                        } else {
                            html = None;
                        }
                    }
                }
                Some(Err(_)) => html = None,
                None => {
                    if let Some(buf) = html.filter(|b| !b.is_empty()) {
                        capture.save(buf).await;
                    }
                    return None;
                }
            }
            Some((next?, (upstream, html, capture)))
        },
    ))
}

pub(super) async fn capture_registered_root(app: &App, agent: &Agent, port: u16) {
    if !matches!(agent.state, AgentState::Starting | AgentState::Running) {
        return;
    }
    let Ok(endpoint) = endpoint(app, agent, port) else {
        return;
    };
    let Ok(url) = url::Url::parse(&endpoint) else {
        return;
    };
    let dir = app.0.config.data_dir.join("agents").join(&agent.id);
    // Registration succeeds even if the listener is not ready. A subsequent
    // successful live preview can create or refresh the capture.
    let Ok(response) = app
        .0
        .service_client
        .get(url.clone())
        .header("accept", "text/html")
        .timeout(Duration::from_secs(2))
        .send()
        .await
    else {
        return;
    };
    if capturable(&response) {
        let capture = Capture::new(&app.0.service_client, url, dir, port);
        let _ = axum::body::to_bytes(capture_body(response, capture), MAX_CAPTURE_BYTES).await;
    }
}
