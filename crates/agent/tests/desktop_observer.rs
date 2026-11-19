//! Exercise the immutable executable entry point on machines without X11.
//! The real rootfs integration verifies screen pixels on the actual X server.
#![cfg(unix)]
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::tempdir;
use tokio::{process::Command, sync::Notify};

const TOKEN: &str = "fake-observer-credential";
const JPEG: &[u8] = include_bytes!("fixtures/screen.jpg");

struct Fixture {
    demands: AtomicUsize,
    uploads: Mutex<Vec<(String, Vec<u8>)>>,
    errors: Mutex<Vec<String>>,
    captured: PathBuf,
    done: Notify,
}

async fn service(State(f): State<Arc<Fixture>>, req: Request) -> Response {
    if req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        != Some("Bearer fake-observer-credential")
    {
        f.errors
            .lock()
            .unwrap()
            .push("missing credential".to_owned());
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match (req.method().as_str(), req.uri().path()) {
        ("GET", "/env/v2/desktop/demand") => {
            let i = f.demands.fetch_add(1, Ordering::Relaxed);
            if i <= 2 && f.captured.exists() {
                f.errors
                    .lock()
                    .unwrap()
                    .push("captured without a valid ticket".into());
            }
            match i {
                0 => StatusCode::NO_CONTENT.into_response(),
                1 => Response::new(Body::from("x".repeat(3000))),
                2 => axum::Json(json!({"ticket":"first"})).into_response(),
                3 => axum::Json(json!({"ticket":"second"})).into_response(),
                _ => StatusCode::NO_CONTENT.into_response(),
            }
        }
        ("POST", "/env/v2/desktop/frame") => {
            if req
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                != Some("image/jpeg")
            {
                f.errors
                    .lock()
                    .unwrap()
                    .push("unexpected frame content type".into());
            }
            let ticket = req
                .headers()
                .get("x-survive-desktop-ticket")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let body = to_bytes(req.into_body(), 128 * 1024).await.unwrap();
            let mut uploads = f.uploads.lock().unwrap();
            uploads.push((ticket, body.to_vec()));
            if uploads.len() == 1 {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            f.done.notify_one();
            StatusCode::NO_CONTENT.into_response()
        }
        _ => {
            f.errors
                .lock()
                .unwrap()
                .push(format!("unexpected {} {}", req.method(), req.uri()));
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn observer_only_captures_on_ticket_and_never_replays_failed_upload_or_opens_work() {
    let tmp = tempdir().unwrap();
    let expected = tmp.path().join("expected.jpg");
    fs::write(&expected, JPEG).unwrap();
    executable(
        &tmp.path().join("xdotool"),
        "#!/bin/sh\nset -eu\ntest -z \"${SURVIVE_TOKEN:-}\"\ntest \"$1\" = getdisplaygeometry\necho '16 16'\n",
    );
    executable(
        &tmp.path().join("scrot"),
        "#!/bin/sh\nset -eu\ntest -z \"${SURVIVE_TOKEN:-}\"\necho \"$PWD\" >> \"$CAPTURE_DIRS\"\n/bin/cp \"$EXPECTED_JPEG\" capture.jpg\n",
    );
    let f = Arc::new(Fixture {
        demands: AtomicUsize::new(0),
        uploads: Mutex::default(),
        errors: Mutex::default(),
        captured: tmp.path().join("captures"),
        done: Notify::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(service).with_state(f.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let work = tmp.path().join("absent-work");
    let mut worker = Command::new(env!("CARGO_BIN_EXE_survive-agent"))
        .arg("--desktop-observer")
        .env("DISPLAY", ":0")
        .env("PATH", tmp.path())
        .env("SURVIVE_TOKEN", TOKEN)
        .env("SURVIVE_ENV_URL", &url)
        .env("SURVIVE_WORK_DIR", &work)
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("CAPTURE_DIRS", &f.captured)
        .env("EXPECTED_JPEG", &expected)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), f.done.notified())
        .await
        .unwrap();
    worker.kill().await.unwrap();
    server.abort();
    assert!(f.errors.lock().unwrap().is_empty());
    assert_eq!(
        f.uploads.lock().unwrap().as_slice(),
        [
            ("first".to_owned(), JPEG.to_vec()),
            ("second".to_owned(), JPEG.to_vec())
        ]
    );
    assert!(!work.exists());
    let captures = fs::read_to_string(&f.captured).unwrap();
    assert_eq!(captures.lines().count(), 2);
    for dir in captures.lines() {
        assert!(dir.contains("survive-desktop-") && !Path::new(dir).exists());
    }
}
