//! On-demand frames from the observer in an organism's own desktop.
//! Waiters share one capture. Only the last observed frame is retained.
use crate::{
    ApiError, App,
    api::{environment_agent, owned_agent},
};
use axum::{
    Json,
    body::{Bytes, to_bytes},
    extract::{ConnectInfo, Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use survive_core::{Agent, AgentState, MAX_FRAME_BYTES, is_desktop_jpeg, new_id};
use tokio::{
    sync::watch,
    time::{Instant, timeout, timeout_at},
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);
type Room = watch::Sender<Option<Arc<Capture>>>;

#[derive(Default)]
pub struct DesktopSessions {
    rooms: Mutex<BTreeMap<(String, u64), Weak<Room>>>,
    // The first capture of a generation needs to live between the guest's poll
    // and upload even if no creator is currently observing.
    initial: Mutex<BTreeMap<String, (Instant, Arc<Room>)>>,
}
struct Capture {
    ticket: String,
    until: Instant,
    taken: AtomicBool,
    uploading: tokio::sync::Mutex<()>,
    result: watch::Sender<Option<Bytes>>,
}
impl Capture {
    fn new() -> Self {
        Self {
            ticket: new_id(),
            until: Instant::now() + CAPTURE_TIMEOUT,
            taken: AtomicBool::new(false),
            uploading: Default::default(),
            result: watch::channel(None).0,
        }
    }
}
impl DesktopSessions {
    fn room(&self, agent: &Agent) -> Result<Arc<Room>, ApiError> {
        if agent.state != AgentState::Running {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "organism desktop is stopped".into(),
            ));
        }
        let mut rooms = self.rooms.lock().expect("desktop session lock");
        rooms.retain(|_, room| room.strong_count() > 0);
        let entry = rooms
            .entry((agent.id.clone(), agent.generation))
            .or_default();
        if let Some(room) = entry.upgrade() {
            return Ok(room);
        }
        let room = Arc::new(watch::channel(None).0);
        *entry = Arc::downgrade(&room);
        Ok(room)
    }
}

pub(crate) async fn observe(
    State(app): State<App>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    let agent = owned_agent(&app, &h, id).await?;
    let room = app.0.desktop.room(&agent)?;
    let mut pending = None;
    room.send_if_modified(|slot| {
        if let Some(c) = slot.as_ref()
            && c.until > Instant::now()
            && c.result.borrow().is_none()
        {
            pending = Some(c.clone());
            return false;
        }
        let c = Arc::new(Capture::new());
        *slot = Some(c.clone());
        pending = Some(c);
        true
    });
    let capture = pending.expect("capture selected under lock");
    let mut frame = capture.result.subscribe();
    let image = timeout_at(capture.until, frame.wait_for(|v| v.is_some())).await;
    if let Ok(Ok(frame)) = image
        && let Some(bytes) = frame.as_ref()
    {
        return Ok((
            [
                ("content-type", "image/jpeg"),
                ("x-content-type-options", "nosniff"),
            ],
            bytes.clone(),
        )
            .into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
pub(crate) async fn demand(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    let agent = environment_agent(&app, &h, peer).await?;
    let room = app.0.desktop.room(&agent)?;
    let path = app
        .0
        .config
        .data_dir
        .join("desktop-snapshots")
        .join(&agent.id);
    use tokio::io::AsyncReadExt;
    let saved_generation = match tokio::fs::File::open(path).await {
        Ok(mut file) => file.read_u64().await.ok(),
        Err(_) => None,
    };
    if saved_generation != Some(agent.generation) {
        let mut pins = app
            .0
            .desktop
            .initial
            .lock()
            .expect("desktop startup captures");
        pins.retain(|_, (until, _)| *until > Instant::now());
        room.send_if_modified(|slot| {
            if slot.as_ref().is_some_and(|c| c.until > Instant::now()) {
                return false;
            }
            *slot = Some(Arc::new(Capture::new()));
            true
        });
        if let Some(c) = room.borrow().as_ref() {
            pins.insert(agent.id.clone(), (c.until, room.clone()));
        }
    }
    let mut offered = room.subscribe();
    let next = async {
        loop {
            if let Some(c) = offered.borrow_and_update().as_ref()
                && c.until > Instant::now()
                && !c.taken.swap(true, Ordering::AcqRel)
            {
                return Some(c.ticket.clone());
            }
            offered.changed().await.ok()?;
        }
    };
    Ok(match timeout(Duration::from_secs(20), next).await {
        Ok(Some(ticket)) => Json(json!({"ticket":ticket})).into_response(),
        _ => StatusCode::NO_CONTENT.into_response(),
    })
}
pub(crate) async fn upload(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Result<StatusCode, ApiError> {
    let agent = environment_agent(&app, req.headers(), peer).await?;
    let room = app.0.desktop.room(&agent)?;
    let capture = room
        .borrow()
        .clone()
        .filter(|c| {
            c.until > Instant::now()
                && c.taken.load(Ordering::Acquire)
                && req
                    .headers()
                    .get("x-survive-desktop-ticket")
                    .and_then(|h| h.to_str().ok())
                    == Some(c.ticket.as_str())
        })
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, "no matching capture request".into()))?;
    if req
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        != Some("image/jpeg")
    {
        return Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected image/jpeg".into(),
        ));
    }
    let bytes = to_bytes(req.into_body(), MAX_FRAME_BYTES)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::PAYLOAD_TOO_LARGE,
                "desktop frame too large".into(),
            )
        })?;
    if !is_desktop_jpeg(&bytes) {
        return Err(ApiError::bad("invalid desktop JPEG or dimensions"));
    }
    let _guard = capture.uploading.lock().await;
    if capture.result.borrow().is_some() || Instant::now() >= capture.until {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "capture already completed or expired".into(),
        ));
    }
    let dir = app.0.config.data_dir.join("desktop-snapshots");
    let id = agent.id.clone();
    let generation = agent.generation;
    let image = bytes.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&dir)?;
        let mut file = tempfile::NamedTempFile::new_in(&dir)?;
        file.write_all(&generation.to_be_bytes())?;
        file.write_all(&image)?;
        file.persist(dir.join(id))?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(|e| ApiError::internal(e.to_string()))?;
    capture.result.send_replace(Some(bytes));
    let mut pins = app
        .0
        .desktop
        .initial
        .lock()
        .expect("desktop startup captures");
    if pins
        .get(&agent.id)
        .is_some_and(|(_, initial)| Arc::ptr_eq(initial, &room))
    {
        pins.remove(&agent.id);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn snapshot(
    State(app): State<App>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    let agent = owned_agent(&app, &h, id).await?;
    let path = app
        .0
        .config
        .data_dir
        .join("desktop-snapshots")
        .join(agent.id);
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        Err(e) => return Err(ApiError::internal(e.to_string())),
    };
    use tokio::io::AsyncReadExt;
    let meta = file
        .metadata()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let timestamp = meta
        .modified()
        .and_then(|v| {
            v.duration_since(std::time::UNIX_EPOCH)
                .map_err(std::io::Error::other)
        })
        .map_err(|e| ApiError::internal(e.to_string()))?
        .as_millis()
        .to_string();
    file.read_u64()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if !is_desktop_jpeg(&bytes) {
        return Err(ApiError::internal("invalid saved desktop frame"));
    }
    Ok((
        [
            ("content-type", "image/jpeg"),
            ("x-content-type-options", "nosniff"),
            ("x-survive-captured-at", timestamp.as_str()),
        ],
        bytes,
    )
        .into_response())
}
