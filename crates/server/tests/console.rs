use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use survive_core::{ExperimentInput, Store, Usage, token_hash};
use survive_gateway::Gateway;
use survive_server::{App, Inner, api, config::Config, console::RuntimeReadiness};
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt;

const TOKEN: &str = "synthetic-console-creator-token-00001";
fn fixture(data: &std::path::Path, assets: Option<&std::path::Path>) -> App {
    let config: Config = serde_json::from_value(json!({
        "data_dir":data,"listen":"127.0.0.1:0","env_listen":"127.0.0.1:0","test_mode":true,
        "console_dir":assets,"creators":[{"id":"owner","token_env":"NOT_A_REAL_KEY"}],
        "gateway":{"providers":[{"name":"public-route","base_url":"http://127.0.0.1:9/v1","models":{"default":"private-provider-model"}}]},
        "judge":{"api_key_env":"NOT_A_REAL_KEY"}
    })).unwrap();
    let store = Store::open(data.join("console.redb")).unwrap();
    App(Arc::new(Inner {
        gateway: Gateway::new(config.gateway.clone(), store.clone()).unwrap(),
        config,
        store,
        sandbox: None,
        service_client: reqwest::Client::new(),
        creators: BTreeMap::from([("owner".into(), token_hash(TOKEN))]),
        lifecycle_lock: Mutex::new(()),
        runtime: RwLock::new(RuntimeReadiness::initial(true)),
    }))
}
fn experiment(name: &str, count: u32) -> ExperimentInput {
    serde_json::from_value(json!({
        "name":name,"goal":"Create an artifact","initial_prompt":"Act independently","count":count,
        "initial_tokens":10000,"reward_budget":50000,
        "rules":[{"id":"complete","instructions":"Is the work correct?","tokens":50}],
        "evidence_sources":[{"id":"source","url":"https://example.invalid/metric","headers":{"Authorization":"source-secret-value"},"event_secret_hash":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"}]
    })).unwrap()
}
async fn get(app: &App, path: &str, auth: bool) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().uri(path);
    if auth {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    let response = api::owner_router(app.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

#[tokio::test]
async fn observer_projections_enforce_ownership_and_strip_implementation_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let app = fixture(temp.path(), None);
    let (own, agents) = app
        .0
        .store
        .create_experiment("owner", experiment("Owned experiment", 2))
        .unwrap();
    let (other, _) = app
        .0
        .store
        .create_experiment("other", experiment("Do not disclose this", 4))
        .unwrap();
    let id = &agents[0].0.id;
    app.0
        .store
        .reserve(id, "settled-one", "private-provider-name", 100)
        .unwrap();
    app.0
        .store
        .settle(
            "settled-one",
            Usage {
                input_tokens: 70,
                output_tokens: 30,
            },
            None,
        )
        .unwrap();
    for path in ["/api/v1/overview", "/api/v1/agents", "/api/v1/topology"] {
        assert_eq!(get(&app, path, false).await.0, StatusCode::UNAUTHORIZED);
        let (status, bytes) = get(&app, path, true).await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(bytes).unwrap();
        for secret in [
            &other.id,
            "Do not disclose this",
            "source-secret-value",
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "token_hash",
            "private-provider-model",
            "NOT_A_REAL_KEY",
        ] {
            assert!(!text.contains(secret), "{path} leaked {secret}");
        }
    }
    let (_, bytes) = get(&app, "/api/v1/overview", true).await;
    let overview: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(overview["summary"]["agents"], 2);
    assert_eq!(overview["summary"]["used"], 100);
    assert_eq!(overview["summary"]["available"], 19900);
    assert_eq!(overview["system"]["runtime"]["available"], false);
    assert_eq!(overview["system"]["llm"]["health_verified"], false);
    assert_eq!(overview["system"]["creator_mode"], "create_and_grant");
    let (_, bytes) = get(
        &app,
        &format!(
            "/api/v1/agents?experiment_id={}&q=owned&state=queued&limit=1",
            own.id
        ),
        true,
    )
    .await;
    let rows: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(rows["total"], 2);
    assert_eq!(rows["agents"].as_array().unwrap().len(), 1);
    assert_eq!(
        get(
            &app,
            &format!("/api/v1/topology?experiment_id={}", other.id),
            true
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, "/api/v1/agents?state=imaginary", true).await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, metrics) = get(&app, "/metrics", true).await;
    assert!(
        String::from_utf8(metrics)
            .unwrap()
            .contains("survive_agents 2\n")
    );
}

#[tokio::test]
async fn topology_labels_configured_permissions_separately_from_observed_usage() {
    let temp = tempfile::tempdir().unwrap();
    let app = fixture(temp.path(), None);
    let (_, agents) = app
        .0
        .store
        .create_experiment("owner", experiment("network", 1))
        .unwrap();
    let id = &agents[0].0.id;
    let (_, before) = get(&app, "/api/v1/topology", true).await;
    let before: Value = serde_json::from_slice(&before).unwrap();
    assert!(
        before["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|edge| edge["basis"] == "configured")
    );
    app.0.store.reserve(id, "consumption", "mock", 25).unwrap();
    app.0
        .store
        .settle(
            "consumption",
            Usage {
                input_tokens: 20,
                output_tokens: 5,
            },
            None,
        )
        .unwrap();
    let (_, after) = get(&app, "/api/v1/topology", true).await;
    let after: Value = serde_json::from_slice(&after).unwrap();
    let observed = after["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["basis"] == "observed")
        .collect::<Vec<_>>();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0]["kind"], "settled_llm_tokens");
    assert_eq!(observed[0]["count"], 25);
}

#[tokio::test]
async fn console_serves_wasm_with_csp_without_swallowing_api_errors_or_exposing_files() {
    let temp = tempfile::tempdir().unwrap();
    let assets = temp.path().join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("index.html"), "<html>console</html>").unwrap();
    std::fs::write(assets.join("client.wasm"), b"\0asm").unwrap();
    std::fs::write(temp.path().join("private.html"), "host secret").unwrap();
    std::fs::write(assets.join("credential.toml"), "not a frontend asset").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(temp.path().join("private.html"), assets.join("escape.html"))
        .unwrap();
    std::fs::write(assets.join("preview.html"), "<html>frame</html>").unwrap();
    let app = fixture(temp.path(), Some(&assets));
    let frame = api::owner_router(app.clone())
        .oneshot(
            Request::builder()
                .uri("/console/preview.html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let policy = frame.headers()["content-security-policy"].to_str().unwrap();
    for required in [
        "sandbox allow-scripts allow-downloads",
        "connect-src 'none'",
        "frame-ancestors 'self'",
    ] {
        assert!(policy.contains(required));
    }
    assert!(!policy.contains("allow-same-origin"));
    assert_eq!(get(&app, "/console/", false).await.0, StatusCode::OK);
    let request = Request::builder()
        .uri("/console/client.wasm")
        .body(Body::empty())
        .unwrap();
    let response = api::owner_router(app.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/wasm");
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("wasm-unsafe-eval")
    );
    for path in [
        "/console/credential.toml",
        "/console/%2e%2e/private.html",
        "/api/v1/not-a-route",
        "/console/escape.html",
    ] {
        assert_eq!(
            get(&app, path, false).await.0,
            StatusCode::NOT_FOUND,
            "unexpected public asset {path}"
        );
    }
}

#[tokio::test]
async fn latest_activity_is_bounded_and_newest_first_without_truncating_history() {
    let temp = tempfile::tempdir().unwrap();
    let app = fixture(temp.path(), None);
    let (_, agents) = app
        .0
        .store
        .create_experiment("owner", experiment("activity", 1))
        .unwrap();
    let id = &agents[0].0.id;
    let mut latest = None;
    for sequence in 0..40 {
        latest = Some(
            app.0
                .store
                .record_event(id, "fixture", json!({"sequence":sequence}))
                .unwrap(),
        );
    }
    let (_, bytes) = get(&app, "/api/v1/overview", true).await;
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let events = value["recent_events"].as_array().unwrap();
    assert_eq!(events.len(), 30);
    assert_eq!(events[0]["id"], latest.unwrap().id);
    assert_eq!(app.0.store.events(id, None, 100).unwrap().len(), 41);
}

#[tokio::test]
async fn peer_discovery_never_exposes_other_experiments_and_rejects_invalid_ports() {
    let temp = tempfile::tempdir().unwrap();
    let app = fixture(temp.path(), None);
    let (_, local) = app
        .0
        .store
        .create_experiment("owner", experiment("local", 2))
        .unwrap();
    let (_, foreign) = app
        .0
        .store
        .create_experiment("owner", experiment("foreign", 1))
        .unwrap();
    for (agent, _) in local.iter().chain(&foreign) {
        app.0
            .store
            .set_agent_state(&agent.id, survive_core::AgentState::Running, None)
            .unwrap();
        app.0
            .store
            .register_service(
                &agent.id,
                survive_core::ServiceRegistration {
                    port: 8000,
                    name: "artifact".into(),
                },
            )
            .unwrap();
    }
    let request = Request::builder()
        .uri("/env/v1/peers")
        .header("authorization", format!("Bearer {}", local[0].1))
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:4567".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    let response = api::environment_router(app.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["peers"].as_array().unwrap().len(), 1);
    assert_eq!(value["peers"][0]["agent"]["id"], local[1].0.id);
    assert!(!String::from_utf8_lossy(&bytes).contains(&foreign[0].0.id));
    assert!(value["peers"][0]["agent"].get("token_hash").is_none());
    let request = Request::builder()
        .method("POST")
        .uri("/env/v1/services")
        .header("authorization", format!("Bearer {}", local[0].1))
        .header("content-type", "application/json")
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:4567".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(r#"{"port":0,"name":"invalid service"}"#))
        .unwrap();
    let response = api::environment_router(app).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stopped_services_show_only_the_registered_ports_saved_site() {
    use survive_core::{AgentState, ServiceRegistration};

    let temp = tempfile::tempdir().unwrap();
    let app = fixture(temp.path(), None);
    let (_, agents) = app
        .0
        .store
        .create_experiment("owner", experiment("saved site", 1))
        .unwrap();
    let id = &agents[0].0.id;
    app.0
        .store
        .set_agent_state(id, AgentState::Suspended, None)
        .unwrap();
    for port in [8080, 8081] {
        app.0
            .store
            .register_service(
                id,
                ServiceRegistration {
                    name: "site".into(),
                    port,
                },
            )
            .unwrap();
    }
    let archive = temp.path().join("agents").join(id).join("previews/8080");
    std::fs::create_dir_all(archive.join("assets")).unwrap();
    std::fs::write(archive.join("index.html"), "<h1>Saved site</h1>").unwrap();
    std::fs::write(archive.join("assets/demo.csv"), "id,amount").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(temp.path(), archive.join("outside")).unwrap();
    let path = format!("/services/{id}/8080/");
    let (status, root) = get(&app, &path, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(root, b"<h1>Saved site</h1>");
    assert_eq!(get(&app, &path, false).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        get(&app, &format!("{path}assets/demo.csv"), true).await.1,
        b"id,amount"
    );
    let request = Request::builder()
        .method("HEAD")
        .uri(&path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = api::owner_router(app.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.headers()["x-survive-preview"], "archive");
    assert_eq!(response.headers()["content-length"], root.len().to_string());
    assert!(
        to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty()
    );
    for relative in [
        "%2e%2e/index.html",
        "outside/console.redb",
        ".private",
        "missing",
    ] {
        assert_eq!(
            get(&app, &format!("{path}{relative}"), true).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        get(&app, &format!("/services/{id}/8081/"), true).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        get(&app, &format!("/services/{id}/8082/"), true).await.0,
        StatusCode::FORBIDDEN
    );
    let (_, bytes) = get(&app, &format!("/api/v2/organisms/{id}/services"), true).await;
    let list: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        list["services"][0]["preview"],
        json!({"mode":"archive","entry":"index.html","detail":"static-site"})
    );
    assert_eq!(
        list["services"][1]["preview"],
        json!({"mode":"unavailable","entry":null,"detail":null})
    );
    let guest_work = temp.path().join("agents").join(id).join("work");
    std::fs::create_dir_all(guest_work.join("public/8081")).unwrap();
    std::fs::write(
        guest_work.join("public/8081/index.html"),
        "explicit public site",
    )
    .unwrap();
    assert_eq!(
        get(&app, &format!("/services/{id}/8081/"), true).await.1,
        b"explicit public site"
    );
    #[cfg(unix)]
    {
        std::fs::write(guest_work.join("private.txt"), "private fixture").unwrap();
        std::os::unix::fs::symlink("../../private.txt", guest_work.join("public/8081/leak.txt"))
            .unwrap();
        assert_eq!(
            get(&app, &format!("/services/{id}/8081/leak.txt"), true)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    let (_, foreign) = app
        .0
        .store
        .create_experiment("another creator", experiment("foreign", 1))
        .unwrap();
    assert_eq!(
        get(&app, &format!("/services/{}/8080/", foreign[0].0.id), true)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let post = Request::builder()
        .method("POST")
        .uri(&path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        api::owner_router(app.clone())
            .oneshot(post)
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        app.0.store.get_agent(id).unwrap().state,
        AgentState::Suspended
    );
}

#[tokio::test]
async fn registration_archives_linked_files_before_any_creator_visits() {
    use axum::response::IntoResponse;
    use survive_core::AgentState;
    let temp = tempfile::tempdir().unwrap();
    let mut app = fixture(temp.path(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    Arc::get_mut(&mut app.0).unwrap().service_client = reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(format!("http://{gateway}")).unwrap())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let large = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let send_large = large.clone();
    let seen = paths.clone();
    let upstream = axum::Router::new().fallback(move |req: Request<Body>| {
        let seen = seen.clone();
        let large = send_large.clone();
        async move {
            assert!(req.headers().get("authorization").is_none());
            assert!(req.headers().get("cookie").is_none());
            let path = req.uri().path().to_string();
            seen.lock().unwrap().push(path.clone());
            if path == "/" && large.load(std::sync::atomic::Ordering::Acquire) {
                let chunks = (0..3).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024 * 1024]));
                return ([("content-type", "text/html")], Body::from_stream(futures_util::stream::iter(chunks))).into_response();
            }
            let (mime, text) = match path.as_str() {
                "/" => ("text/html", r#"<link rel="stylesheet" href="./assets/main.css"><script src="app.js"></script><a href="demo.csv">CSV</a><a download href="demo.csv">Again</a><script src="https://example.invalid/remote.js"></script>"#),
                "/assets/main.css" => ("text/css", "body{color:green}"),
                "/app.js" => ("text/javascript", "window.previewRunning=true;"),
                "/demo.csv" => ("text/csv", "sample,value"),
                _ => return StatusCode::NOT_FOUND.into_response()
            };
            ([("content-type", mime)], text).into_response()
        }
    });
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let store = &app.0.store;
    let env = store
        .create_environment(
            "owner",
            serde_json::from_value(json!({"description":"preview"})).unwrap(),
        )
        .unwrap();
    let agent = store
        .birth_organisms(
            "owner",
            &env.id,
            serde_json::from_value(
                json!({"count":1,"defaults":{"initial_energy":100,"lifetime_seconds":30}}),
            )
            .unwrap(),
        )
        .unwrap()
        .remove(0);
    let id = &agent.id;
    store
        .set_agent_state(id, AgentState::Running, None)
        .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/env/v2/services")
        .header(
            "authorization",
            format!("Bearer {}", store.agent_credential(id).unwrap()),
        )
        .header("content-type", "application/json")
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:4242".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(r#"{"name":"CSV","port":8080}"#))
        .unwrap();
    let response = api::environment_router(app.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *paths.lock().unwrap(),
        ["/", "/assets/main.css", "/app.js", "/demo.csv"]
    );

    let root = format!("/services/{id}/8080/");
    let response = api::owner_router(app.clone())
        .oneshot(
            Request::builder()
                .uri(format!("{root}assets/main.css"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["x-survive-preview"], "live");
    // Chunked HTML over the capture limit must stream in full, while the
    // earlier usable capture survives for the subsequent offline request.
    large.store(true, std::sync::atomic::Ordering::Release);
    let live = api::owner_router(app.clone())
        .oneshot(
            Request::builder()
                .uri(&root)
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        to_bytes(live.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap()
            .len(),
        3 * 1024 * 1024
    );
    server.abort();
    let _ = server.await;
    // Running state can briefly outlive a listener. Its capture is immediately usable.
    assert_eq!(
        get(&app, &format!("{root}app.js"), true).await.1,
        b"window.previewRunning=true;"
    );
    store
        .set_agent_state(id, AgentState::Suspended, None)
        .unwrap();
    for (asset, body) in [
        ("assets/main.css", "body{color:green}"),
        ("demo.csv", "sample,value"),
    ] {
        assert_eq!(
            get(&app, &format!("{root}{asset}"), true).await.1,
            body.as_bytes()
        );
    }
    let response = api::owner_router(app.clone())
        .oneshot(
            Request::builder()
                .uri(&root)
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()["x-survive-preview-detail"],
        "partial-capture"
    );
    assert_eq!(response.headers()["x-survive-preview"], "archive");
    assert_eq!(store.get_agent(id).unwrap().state, AgentState::Suspended);
}

#[tokio::test]
async fn public_guide_links_and_authenticated_examples_use_configured_origin() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = fixture(dir.path(), None);
    let config = &mut Arc::get_mut(&mut app.0).unwrap().config;
    config.public_origin = Some("https://creator.example:9443".to_owned());
    for path in ["/llm.txt", "/llms.txt"] {
        let response = api::owner_router(app.clone())
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "untrusted.example")
                    .header("x-forwarded-host", "untrusted.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = String::from_utf8(
            to_bytes(response.into_body(), 65536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("](https://creator.example:9443/openapi.json)"));
        assert!(text.contains("export SURVIVE_ORIGIN=\"https://creator.example:9443\""));
        assert!(!text.contains("untrusted.example"));
    }
}
