use crate::{ApiError, App};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, net::SocketAddr, path::PathBuf};
use survive_core::*;
use survive_gateway::Protocol;

#[path = "life_api.rs"]
mod life;
#[path = "service_preview.rs"]
mod preview;

pub fn owner_router(app: App) -> Router {
    admission(
        Router::new()
            .merge(life::owner_routes())
            .route("/healthz", get(health))
            .route("/readyz", get(ready))
            .route("/llm.txt", get(llm_doc))
            .route("/llms.txt", get(llm_doc))
            .route("/openapi.json", get(openapi))
            .route("/", get(crate::console::root))
            .route("/console", get(crate::console::root))
            .route("/console/", get(crate::console::index))
            .route("/console/{*path}", get(crate::console::asset))
            .route("/api/v1/overview", get(crate::console::overview))
            .route("/api/v1/topology", get(crate::console::topology))
            .route("/api/v1/agents", get(crate::console::agent_list))
            .route("/api/v1/experiments", get(list_experiments))
            .route("/api/v1/experiments/{id}", get(experiment))
            .route("/api/v1/experiments/{id}/agents", get(agents))
            .route("/api/v1/agents/{id}", get(agent))
            .route("/api/v1/agents/{id}/events", get(events))
            .route("/api/v1/agents/{id}/events/{event}", get(event_detail))
            .route("/api/v1/agents/{id}/ledger", get(ledger))
            .route("/api/v1/agents/{id}/files", get(files))
            .route("/api/v1/agents/{id}/submissions", get(submissions))
            .route("/api/v1/submissions/{id}", get(submission_detail))
            .route("/api/v1/agents/{id}/services", get(services))
            .route("/services/{id}/{port}", get(service_root))
            .route("/services/{id}/{port}/", get(service_root))
            .route("/services/{id}/{port}/{*path}", get(service_proxy))
            .route("/metrics", get(metrics))
            .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
            .with_state(app),
    )
}
pub fn environment_router(app: App) -> Router {
    admission(
        Router::new()
            .merge(life::guest_routes())
            .route("/env/v1/self", get(self_info))
            .route("/env/v1/peers", get(peers))
            .route("/env/v1/events", get(self_events))
            .route("/env/v1/events/{id}", get(self_event_detail))
            .route("/env/v1/ledger", get(self_ledger))
            .route("/env/v1/submissions", get(self_submissions))
            .route("/env/v1/submissions/{id}", get(self_submission))
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/responses", post(responses))
            .route("/v1/responses/{id}", get(response))
            .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
            .with_state(app),
    )
}
fn admission(router: Router) -> Router {
    use std::{collections::BTreeMap, sync::Arc};
    use tokio::sync::{Mutex, Semaphore};
    let global = Arc::new(Semaphore::new(32));
    let peers = Arc::new(Mutex::new(
        BTreeMap::<std::net::IpAddr, Arc<Semaphore>>::new(),
    ));
    router.layer(axum::middleware::from_fn(
        move |request: Request, next: axum::middleware::Next| {
            let global = global.clone();
            let peers = peers.clone();
            async move {
                let private_response = !request.uri().path().starts_with("/console");
                let Ok(_global) = global.try_acquire_owned() else {
                    return ApiError(
                        StatusCode::TOO_MANY_REQUESTS,
                        "HTTP concurrency limit".into(),
                    )
                    .into_response();
                };
                let ip = request
                    .extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|c| c.0.ip())
                    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                let limit = {
                    let mut limits = peers.lock().await;
                    if limits.len() >= 65536 && !limits.contains_key(&ip) {
                        return ApiError(StatusCode::TOO_MANY_REQUESTS, "HTTP peer limit".into())
                            .into_response();
                    };
                    limits
                        .entry(ip)
                        .or_insert_with(|| Arc::new(Semaphore::new(4)))
                        .clone()
                };
                let Ok(_peer) = limit.try_acquire_owned() else {
                    return ApiError(
                        StatusCode::TOO_MANY_REQUESTS,
                        "peer concurrency limit".into(),
                    )
                    .into_response();
                };
                match tokio::time::timeout(std::time::Duration::from_secs(150), next.run(request))
                    .await
                {
                    Ok(mut response) => {
                        if private_response {
                            response.headers_mut().insert(
                                axum::http::header::CACHE_CONTROL,
                                axum::http::HeaderValue::from_static("no-store"),
                            );
                        }
                        response
                    }
                    Err(_) => ApiError(StatusCode::REQUEST_TIMEOUT, "request timeout".into())
                        .into_response(),
                }
            }
        },
    ))
}
fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError(
                StatusCode::UNAUTHORIZED,
                "Bearer credential required".into(),
            )
        })
}
pub(crate) fn owner(app: &App, h: &HeaderMap) -> Result<String, ApiError> {
    let hash = token_hash(bearer(h)?);
    app.0
        .creators
        .iter()
        .find_map(|(id, v)| if *v == hash { Some(id.clone()) } else { None })
        .ok_or_else(ApiError::forbidden)
}
async fn owned_agent(app: &App, h: &HeaderMap, id: String) -> Result<Agent, ApiError> {
    let owner = owner(app, h)?;
    app.db(move |s| {
        let a = s.get_agent(&id)?;
        let actual_owner = match s.organism_profile(&id) {
            Ok(profile) => profile.owner,
            Err(error) if error.to_string().starts_with("not_found:") => {
                s.get_experiment(&a.experiment_id)?.owner
            }
            Err(error) => return Err(error),
        };
        anyhow::ensure!(actual_owner == owner, "forbidden");
        Ok(a)
    })
    .await
    .map_err(|e| {
        if e.1 == "forbidden" {
            ApiError::forbidden()
        } else {
            e
        }
    })
}
pub async fn environment_agent(
    app: &App,
    h: &HeaderMap,
    peer: SocketAddr,
) -> Result<Agent, ApiError> {
    let token = bearer(h)?.to_owned();
    let a = app
        .db(move |s| s.authenticate_agent(&token))
        .await
        .map_err(|_| ApiError::forbidden())?;
    if !(app.0.config.test_mode && peer.ip().is_loopback()) && peer.ip().to_string() != a.source_ip
    {
        return Err(ApiError::forbidden());
    }
    Ok(a)
}
pub(crate) fn public_agent(a: &Agent) -> Value {
    let mut value = serde_json::to_value(a).unwrap();
    value.as_object_mut().unwrap().remove("token_hash");
    value["balance"] = json!(a.balance());
    value["available"] = json!(a.available());
    value
}
pub(crate) fn public_experiment(e: &Experiment) -> Value {
    let mut value = serde_json::to_value(e).unwrap();
    if let Some(sources) = value["config"]["evidence_sources"].as_array_mut() {
        for source in sources {
            source.as_object_mut().unwrap().remove("headers");
            source.as_object_mut().unwrap().remove("event_secret_hash");
        }
    }
    value
}
async fn health() -> Json<Value> {
    Json(json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}))
}
async fn ready(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    app.db(|s| s.list_agents(None)).await?;
    let model_credentials = app.0.config.gateway.providers.iter().any(|p| {
        p.api_key_env.as_ref().is_none_or(|key| {
            std::env::var(key)
                .ok()
                .is_some_and(|v| !v.trim().is_empty())
        })
    });
    let runtime = app.0.runtime.read().await.clone();
    Ok(Json(
        json!({"status":"ready","test_mode":app.0.config.test_mode,"model_credentials_configured":model_credentials,"runtime":runtime}),
    ))
}
async fn llm_doc(State(app): State<App>) -> impl IntoResponse {
    const GUIDE: &str = include_str!("../../../docs/llm.txt");
    // Links placed next to a bearer-token command must come from trusted configuration.
    let guide = app.0.config.public_origin.as_deref().map_or_else(
        || std::borrow::Cow::Borrowed(GUIDE),
        |origin| {
            let text = GUIDE
                .replace("](./openapi.json)", &format!("]({origin}/openapi.json)"))
                .replace("https://survive.example", origin);
            std::borrow::Cow::Owned(text)
        },
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        guide,
    )
}
async fn openapi() -> Json<Value> {
    Json(
        serde_json::from_str(include_str!("../../../docs/openapi.json")).expect("embedded OpenAPI"),
    )
}

async fn list_experiments(State(app): State<App>, h: HeaderMap) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let es = app.db(move |s| s.list_experiments(&who)).await?;
    Ok(Json(
        json!({"experiments":es.iter().map(public_experiment).collect::<Vec<_>>()}),
    ))
}
async fn experiment(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let e = app.db(move |s| s.get_experiment(&id)).await?;
    if e.owner != who {
        return Err(ApiError::forbidden());
    }
    Ok(Json(public_experiment(&e)))
}
async fn agents(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let list = app
        .db(move |s| {
            let e = s.get_experiment(&id)?;
            anyhow::ensure!(e.owner == who, "forbidden");
            s.list_agents(Some(&id))
        })
        .await?;
    Ok(Json(
        json!({"agents":list.iter().map(public_agent).collect::<Vec<_>>()}),
    ))
}
async fn agent(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(public_agent(&owned_agent(&app, &h, id).await?)))
}
#[derive(Deserialize, Default)]
pub struct Page {
    pub after: Option<String>,
    pub limit: Option<usize>,
}
async fn events(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(p): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    owned_agent(&app, &h, id.clone()).await?;
    event_page(&app, id, p).await
}
async fn event_page(app: &App, id: String, p: Page) -> Result<Json<Value>, ApiError> {
    let events = app
        .db(move |s| s.events(&id, p.after.as_deref(), p.limit.unwrap_or(100)))
        .await?;
    let cursor = events.last().map(|e| e.id.clone());
    Ok(Json(json!({"events":events,"next_cursor":cursor})))
}
async fn ledger(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(p): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    owned_agent(&app, &h, id.clone()).await?;
    ledger_value(&app, id, p).await
}
async fn ledger_value(app: &App, id: String, p: Page) -> Result<Json<Value>, ApiError> {
    let (a, (rs, cursor)) = app
        .db(move |s| {
            Ok((
                s.get_agent(&id)?,
                s.reservation_page(&id, p.after.as_deref(), p.limit.unwrap_or(100))?,
            ))
        })
        .await?;
    let mut rs = serde_json::to_value(rs).unwrap();
    for r in rs.as_array_mut().unwrap() {
        r.as_object_mut().unwrap().remove("provider");
    }
    Ok(Json(
        json!({"granted":a.granted,"used":a.used,"reserved":a.reserved,"balance":a.balance(),"available":a.available(),"reservations":rs,"next_cursor":cursor}),
    ))
}
async fn submissions(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(p): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    owned_agent(&app, &h, id.clone()).await?;
    submission_page(&app, id, p).await
}
async fn submission_page(app: &App, id: String, p: Page) -> Result<Json<Value>, ApiError> {
    let (rows, cursor) = app
        .db(move |s| s.submission_page(&id, p.after.as_deref(), p.limit.unwrap_or(8)))
        .await?;
    let rows = rows
        .into_iter()
        .map(|row| {
            let mut value = serde_json::to_value(row).unwrap();
            value.as_object_mut().unwrap().remove("evidence");
            value
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"submissions":rows,"next_cursor":cursor})))
}
async fn submission_detail(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    owner(&app, &h)?;
    let submission = app.db(move |s| s.get_submission(&id)).await?;
    owned_agent(&app, &h, submission.agent_id.clone()).await?;
    Ok(Json(json!(submission)))
}
async fn event_detail(
    State(app): State<App>,
    h: HeaderMap,
    Path((id, event)): Path<(String, String)>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, ApiError> {
    owned_agent(&app, &h, id.clone()).await?;
    event_bytes(&app, id, event, q).await
}
async fn self_event_detail(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, ApiError> {
    let agent = environment_agent(&app, &h, peer).await?;
    event_bytes(&app, agent.id, id, q).await
}
async fn event_bytes(
    app: &App,
    agent: String,
    id: String,
    q: FileQuery,
) -> Result<Json<Value>, ApiError> {
    let (bytes, total) = app
        .db(move |s| s.event_chunk(&agent, &id, q.offset as usize, q.limit))
        .await?;
    Ok(Json(
        json!({"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(&bytes),"total_bytes":total,"next_offset":(q.offset as usize).min(total)+bytes.len()}),
    ))
}
async fn services(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let agent = owned_agent(&app, &h, id.clone()).await?;
    let registrations = app.db(move |s| s.services(&id)).await?;
    preview::service_list(&app, agent, registrations).await
}

#[derive(Deserialize)]
struct FileQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    offset: u64,
    #[serde(default = "file_limit")]
    limit: usize,
}
fn file_limit() -> usize {
    65536
}
async fn files(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, ApiError> {
    owned_agent(&app, &h, id.clone()).await?;
    let root = app.0.config.data_dir.join("agents").join(id).join("work");
    let value = tokio::task::spawn_blocking(move || read_file(root, q))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(Json(value))
}
fn read_file(root: PathBuf, q: FileQuery) -> Result<Value, ApiError> {
    use std::io::{Seek, SeekFrom};
    let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .map_err(|e| ApiError::bad(e.to_string()))?;
    let path = if q.path.is_empty() {
        "."
    } else {
        q.path.as_str()
    };
    if dir
        .metadata(path)
        .map_err(|e| ApiError::bad(e.to_string()))?
        .is_dir()
    {
        let d = dir
            .open_dir(path)
            .map_err(|e| ApiError::bad(e.to_string()))?;
        let mut entries = Vec::new();
        for e in d
            .entries()
            .map_err(|e| ApiError::bad(e.to_string()))?
            .take(1000)
        {
            let e = e.map_err(|e| ApiError::bad(e.to_string()))?;
            entries.push(e.file_name().to_string_lossy().to_string());
        }
        entries.sort();
        return Ok(json!({"path":q.path,"entries":entries}));
    }
    let mut f = open_regular(&dir, path)?;
    f.seek(SeekFrom::Start(q.offset))
        .map_err(|e| ApiError::bad(e.to_string()))?;
    let mut bytes = Vec::new();
    f.take(q.limit.min(1024 * 1024) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(
        json!({"path":q.path,"offset":q.offset,"next_offset":q.offset+bytes.len() as u64,"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(bytes)}),
    )
}
async fn self_info(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let id = a.experiment_id.clone();
    let e = app.db(move |s| s.get_experiment(&id)).await?;
    Ok(Json(
        json!({"agent":public_agent(&a),"balance":a.balance(),"available":a.available(),"experiment":{"id":e.id,"goal":e.config.goal,"model":e.config.model,"rules":e.config.rules,"limits":e.config.limits,"network":e.config.network},"paths":{"prompt":"/work/prompt.md","source":"/work/source","boot":"/work/boot.json"}}),
    ))
}
async fn peers(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let caller = environment_agent(&app, &h, peer).await?;
    let experiment_id = caller.experiment_id.clone();
    let (experiment, peers) = app
        .db(move |store| {
            let experiment = store.get_experiment(&experiment_id)?;
            let mut peers = Vec::new();
            if experiment.config.network.peers && experiment.config.network.services {
                for agent in store.list_agents(Some(&experiment_id))? {
                    if agent.id != caller.id && agent.state == AgentState::Running {
                        let services = store.services(&agent.id)?;
                        peers.push((agent, services));
                    }
                }
            }
            Ok((experiment, peers))
        })
        .await?;
    if !experiment.config.network.peers {
        return Err(ApiError::forbidden());
    }
    let peers = peers
        .into_iter()
        .map(|(agent, services)| {
            let services = services
                .into_iter()
                .filter_map(|service| {
                    let endpoint = if let Some(sandbox) = &app.0.sandbox {
                        sandbox.service_endpoint(&agent.id, service.port).ok()?
                    } else {
                        format!("http://{}:{}/", agent.source_ip, service.port)
                    };
                    Some(json!({"name":service.name,"port":service.port,"url":endpoint}))
                })
                .collect::<Vec<_>>();
            json!({"agent":public_agent(&agent),"services":services})
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"peers":peers,"transport":"direct_http"})))
}
async fn self_events(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(p): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    event_page(&app, a.id, p).await
}
async fn self_ledger(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(p): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    ledger_value(&app, a.id, p).await
}
fn open_regular(dir: &cap_std::fs::Dir, path: &str) -> Result<cap_std::fs::File, ApiError> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = dir
        .open_with(path, &options)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    if !file
        .metadata()
        .map_err(|e| ApiError::bad(e.to_string()))?
        .is_file()
    {
        return Err(ApiError::bad("only regular files are accepted"));
    }
    Ok(file)
}
async fn self_submissions(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(p): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    submission_page(&app, a.id, p).await
}
async fn self_submission(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let s = app.db(move |s| s.get_submission(&id)).await?;
    if s.agent_id != a.id {
        return Err(ApiError::forbidden());
    }
    Ok(Json(json!(s)))
}
async fn restart(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<impl IntoResponse, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let key = a.id.clone();
    if !app
        .db(move |s| s.life_state(&key))
        .await?
        .stop_reasons
        .is_empty()
    {
        return Err(ApiError::bad("depleted"));
    }
    let id = a.id;
    let event_id = id.clone();
    app.db(move |s| s.record_event(&event_id, "restart_requested", json!({})))
        .await?;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let _guard = app.0.lifecycle_lock.lock().await;
        if let Some(sandbox) = &app.0.sandbox {
            if sandbox
                .status(&id)
                .await
                .is_ok_and(|status| status == survive_sandbox::SandboxStatus::Running)
            {
                let _ = crate::lifecycle::checkpoint(&app, &id, true).await;
            }
            if let Err(e) = sandbox.stop(&id).await {
                tracing::error!(agent_id=%id,error=%e,"self restart failed");
                return;
            }
            if crate::lifecycle::checkpoint(&app, &id, false)
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = app
            .db(move |s| s.set_agent_state(&id, AgentState::Queued, None))
            .await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"status":"restart_requested"})),
    ))
}
async fn organism_models(app: &App, id: String) -> Result<Vec<Value>, ApiError> {
    let (environment, birth) = app
        .db(move |store| {
            let p = store.organism_profile(&id)?;
            Ok((store.environment(&p.environment_id)?, p.birth))
        })
        .await?;
    Ok(app.0.gateway.organism_catalog(&environment, &birth))
}
async fn models(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    Ok(Json(
        json!({"object":"list","data":organism_models(&app, a.id).await?}),
    ))
}
async fn chat(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let key = a.id.clone();
    app.db(move |s| s.organism_profile(&key)).await?;
    Ok(app
        .0
        .gateway
        .completion(&a.id, Protocol::Chat, body)
        .await
        .unwrap_or_else(IntoResponse::into_response))
}
async fn responses(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let key = a.id.clone();
    app.db(move |s| s.organism_profile(&key)).await?;
    Ok(app
        .0
        .gateway
        .completion(&a.id, Protocol::Responses, body)
        .await
        .unwrap_or_else(IntoResponse::into_response))
}
async fn response(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    Ok(app
        .0
        .gateway
        .response(&a.id, &id)
        .await
        .unwrap_or_else(IntoResponse::into_response))
}

async fn service_root(
    State(app): State<App>,
    Path((id, port)): Path<(String, u16)>,
    request: Request,
) -> Result<Response, ApiError> {
    proxy(&app, id, port, String::new(), request).await
}
async fn service_proxy(
    State(app): State<App>,
    Path((id, port, path)): Path<(String, u16, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    proxy(&app, id, port, path, request).await
}
async fn proxy(
    app: &App,
    id: String,
    port: u16,
    path: String,
    request: Request,
) -> Result<Response, ApiError> {
    let a = owned_agent(app, request.headers(), id.clone()).await?;
    let services = app.db(move |s| s.services(&id)).await?;
    if !services.iter().any(|s| s.port == port) {
        return Err(ApiError::forbidden());
    }
    let agent_dir = app.0.config.data_dir.join("agents").join(&a.id);
    let head = request.method() == axum::http::Method::HEAD;
    if a.state == AgentState::Running {
        let endpoint = preview::endpoint(app, &a, port)?;
        match live_service(app, endpoint, &path, request, &agent_dir, port).await {
            Ok(response) => return Ok(response),
            Err(error) if error.0 == StatusCode::BAD_GATEWAY => {}
            Err(error) => return Err(error),
        }
    }
    preview::serve_archive(agent_dir, port, path, head).await
}

async fn live_service(
    app: &App,
    endpoint: String,
    path: &str,
    request: Request,
    agent_dir: &std::path::Path,
    port: u16,
) -> Result<Response, ApiError> {
    let capture = path.is_empty()
        && request.uri().query().is_none()
        && request.method() == axum::http::Method::GET
        && !request.headers().contains_key("range");
    let mut url = url::Url::parse(&endpoint).map_err(|e| ApiError::bad(e.to_string()))?;
    let prefix = url.path().trim_end_matches('/').to_owned();
    url.set_path(&format!("{prefix}/{path}"));
    url.set_query(request.uri().query());
    let mut outbound = app
        .0
        .service_client
        .request(request.method().clone(), url.clone());
    for header in ["content-type", "accept", "range", "if-none-match"] {
        if let Some(value) = request.headers().get(header) {
            outbound = outbound.header(header, value);
        }
    }
    let body = axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024)
        .await
        .map_err(|_| ApiError::bad("service request body too large"))?;
    let upstream = outbound
        .body(body)
        .send()
        .await
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "service unreachable".into()))?;
    let mut response = Response::builder().status(upstream.status());
    if let Some(content_type) = upstream.headers().get("content-type") {
        response = response.header("content-type", content_type);
    }
    let body = if capture && preview::capturable(&upstream) {
        let target = preview::Capture::new(&app.0.service_client, url, agent_dir.to_owned(), port);
        preview::capture_body(upstream, target)
    } else {
        Body::from_stream(upstream.bytes_stream())
    };
    preview::response_headers(response, "live")
        .body(body)
        .map_err(|e| ApiError::internal(e.to_string()))
}
async fn metrics(State(app): State<App>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let creator = owner(&app, &h)?;
    let (agents, pending, organisms) = app
        .db(move |s| {
            let experiments = s
                .list_experiments(&creator)?
                .into_iter()
                .map(|e| e.id)
                .collect::<std::collections::BTreeSet<_>>();
            let agents = s
                .list_agents(None)?
                .into_iter()
                .filter(|a| experiments.contains(&a.experiment_id))
                .collect::<Vec<_>>();
            let mut pending = 0;
            for agent in &agents {
                let counts = s.submission_counts(&agent.id)?;
                pending += counts.get("pending").copied().unwrap_or(0)
                    + counts.get("evaluating").copied().unwrap_or(0);
            }
            let organisms = s
                .profiles(Some(&creator))?
                .iter()
                .map(|p| s.get_agent(&p.id))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok((agents, pending, organisms))
        })
        .await?;
    let mut text = format!(
        "# TYPE survive_agents gauge\nsurvive_agents {}\n# TYPE survive_running_agents gauge\nsurvive_running_agents {}\n# TYPE survive_pending_submissions gauge\nsurvive_pending_submissions {}\n# TYPE survive_tokens_used counter\nsurvive_tokens_used {}\n",
        agents.len(),
        agents
            .iter()
            .filter(|a| a.state == AgentState::Running)
            .count(),
        pending,
        agents.iter().map(|a| a.used as u128).sum::<u128>()
    );
    text.push_str(&format!(
        "# TYPE survive_organisms gauge\nsurvive_organisms {}\n# TYPE survive_running_organisms gauge\nsurvive_running_organisms {}\n# TYPE survive_energy_used counter\nsurvive_energy_used {}\n",
        organisms.len(), organisms.iter().filter(|a|a.state==AgentState::Running).count(),
        organisms.iter().map(|a|u128::from(a.used)).sum::<u128>()
    ));
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        text,
    ))
}
