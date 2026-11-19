//! Creator-scoped observer projections and the Rust/WASM console assets.
//!
//! Topology edges describe configured capabilities or durable observations; they
//! are never presented as live packet captures. All mutation stays in the birth API.
use crate::{ApiError, App, api};
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Component, PathBuf},
};
use survive_core::{Agent, AgentState, Experiment, Store, now_ms};

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeReadiness {
    pub backend: String,
    pub available: bool,
    pub status: String,
    pub message: String,
    pub checked_at: Option<u64>,
}
impl RuntimeReadiness {
    pub fn initial(test_mode: bool) -> Self {
        Self {
            backend: if test_mode { "test" } else { "youki" }.into(),
            available: false,
            status: if test_mode { "test_mode" } else { "checking" }.into(),
            message: if test_mode {
                "Test mode: no individual execution environment is started."
            } else {
                "Waiting for the youki host isolation capability check."
            }
            .into(),
            checked_at: None,
        }
    }
    pub(crate) fn checked(result: anyhow::Result<()>) -> Self {
        let (available, status, message) = match result {
            Ok(()) => (
                true,
                "ready",
                "youki host isolation prerequisites are available. This check does not start an individual.",
            ),
            Err(error) => {
                let text = error.to_string().to_lowercase();
                tracing::warn!(error=%error, "youki isolation is unavailable; creator console remains available");
                if text.contains("linux") {
                    (
                        false,
                        "unsupported_host",
                        "The production runtime requires a supported Linux host.",
                    )
                } else if text.contains("cgroup")
                    || text.contains("namespace")
                    || text.contains("seccomp")
                {
                    (
                        false,
                        "isolation_unavailable",
                        "Required host isolation controls are unavailable. Individuals remain queued until runtime preflight succeeds.",
                    )
                } else if text.contains("timeout") || text.contains("timed out") {
                    (
                        false,
                        "preflight_timeout",
                        "The runtime capability check timed out. Individuals remain queued until preflight succeeds.",
                    )
                } else {
                    (
                        false,
                        "unavailable",
                        "youki execution preflight failed. Review the private server diagnostics; individuals will not start without isolation.",
                    )
                }
            }
        };
        Self {
            backend: "youki".into(),
            available,
            status: status.into(),
            message: message.into(),
            checked_at: Some(now_ms()),
        }
    }
}

fn configured(name: Option<&str>) -> bool {
    name.is_none_or(|key| {
        std::env::var(key)
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    })
}

pub async fn system(app: &App) -> Value {
    let providers = app
        .0
        .config
        .gateway
        .providers
        .iter()
        .map(|provider| {
            json!({
                "name": provider.name,
                "configured": configured(provider.api_key_env.as_deref()),
                "protocols": provider.protocols,
                "models": provider.models.keys().collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let models = app
        .0
        .config
        .gateway
        .providers
        .iter()
        .flat_map(|p| p.models.keys().cloned())
        .collect::<BTreeSet<_>>();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "test_mode": app.0.config.test_mode,
        "runtime": app.0.runtime.read().await.clone(),
        "llm": {
            "configured": providers.iter().any(|p| p["configured"] == true),
            "models": models,
            "providers": providers,
            "health_verified": false
        },

        "limits": {
            "pool_memory_bytes": app.0.config.sandbox.as_ref().map(|s| s.total_memory_bytes),
            "llm_global_concurrency": app.0.config.gateway.global_concurrency,
            "llm_per_agent_concurrency": app.0.config.gateway.per_agent_concurrency,

        },
        "creator_mode": "create_and_grant",
        "charging": "model_energy_rates"
    })
}

#[derive(Default, Clone, Serialize)]
struct Summary {
    experiments: usize,
    agents: usize,
    running: usize,
    queued: usize,
    starting: usize,
    depleted: usize,
    failed: usize,
    suspended: usize,
    granted: u64,
    used: u64,
    reserved: u64,
    available: u64,
    balance: u64,
    reward_remaining: u64,
    pending_submissions: u64,
    awarded_submissions: u64,
    rejected_submissions: u64,
    expired_submissions: u64,
}
impl Summary {
    fn add_agent(&mut self, agent: &Agent) {
        self.agents += 1;
        match agent.state {
            AgentState::Running => self.running += 1,
            AgentState::Queued => self.queued += 1,
            AgentState::Starting => self.starting += 1,
            AgentState::Depleted => self.depleted += 1,
            AgentState::Failed => self.failed += 1,
            AgentState::Suspended => self.suspended += 1,
        }
        self.granted = self.granted.saturating_add(agent.granted);
        self.used = self.used.saturating_add(agent.used);
        self.reserved = self.reserved.saturating_add(agent.reserved);
        self.available = self.available.saturating_add(agent.available());
        self.balance = self.balance.saturating_add(agent.balance());
    }
    fn add_submissions(&mut self, counts: &BTreeMap<String, u64>) {
        self.pending_submissions += counts.get("pending").copied().unwrap_or(0)
            + counts.get("evaluating").copied().unwrap_or(0);
        self.awarded_submissions += counts.get("awarded").copied().unwrap_or(0);
        self.rejected_submissions += counts.get("rejected").copied().unwrap_or(0);
        self.expired_submissions += counts.get("expired").copied().unwrap_or(0);
    }
}

fn owned_data(
    store: &Store,
    creator: &str,
    experiment_id: Option<&str>,
) -> anyhow::Result<(Vec<Experiment>, Vec<Agent>)> {
    let mut experiments = store.list_experiments(creator)?;
    if let Some(id) = experiment_id {
        // Do not disclose whether an unknown id belongs to another creator.
        anyhow::ensure!(
            experiments.iter().any(|e| e.id == id),
            "not_found: experiment"
        );
        experiments.retain(|e| e.id == id);
    }
    experiments.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    let owned = experiments
        .iter()
        .map(|e| e.id.as_str())
        .collect::<BTreeSet<_>>();
    let agents = store
        .list_agents(experiment_id)?
        .into_iter()
        .filter(|a| owned.contains(a.experiment_id.as_str()))
        .collect();
    Ok((experiments, agents))
}

pub async fn overview(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let creator = api::owner(&app, &headers)?;
    let owner = creator.clone();
    let mut value = app
        .db(move |store| {
            let (experiments, agents) = owned_data(&store, &owner, None)?;
            let mut summary = Summary {
                experiments: experiments.len(),
                ..Default::default()
            };
            let mut per_experiment = BTreeMap::<String, Summary>::new();
            let mut activity = Vec::new();
            for agent in &agents {
                let stats = per_experiment
                    .entry(agent.experiment_id.clone())
                    .or_default();
                summary.add_agent(agent);
                stats.add_agent(agent);
                let counts = store.submission_counts(&agent.id)?;
                summary.add_submissions(&counts);
                stats.add_submissions(&counts);
                activity.extend(store.recent_events(&agent.id, 30)?);
                // Keep the response and intermediate activity buffer bounded for large populations.
                activity.sort_by(|a, b| b.id.cmp(&a.id));
                activity.truncate(30);
            }
            let experiments = experiments
                .iter()
                .map(|experiment| {
                    summary.reward_remaining = summary
                        .reward_remaining
                        .saturating_add(experiment.reward_remaining);
                    let mut value = api::public_experiment(experiment);
                    let mut stats = per_experiment.remove(&experiment.id).unwrap_or_default();
                    stats.reward_remaining = experiment.reward_remaining;
                    value["stats"] = json!(stats);
                    value
                })
                .collect::<Vec<_>>();
            Ok(json!({
                "creator_id": creator,
                "generated_at": now_ms(),
                "summary": summary,
                "experiments": experiments,
                "agents": agents.iter().map(api::public_agent).collect::<Vec<_>>(),
                "recent_events": activity
            }))
        })
        .await?;
    value["system"] = system(&app).await;
    Ok(Json(value))
}

#[derive(Deserialize, Default)]
pub struct AgentQuery {
    pub experiment_id: Option<String>,
    pub state: Option<String>,
    pub q: Option<String>,
    pub after: Option<u32>,
    pub limit: Option<usize>,
}
pub async fn agent_list(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<AgentQuery>,
) -> Result<Json<Value>, ApiError> {
    let creator = api::owner(&app, &headers)?;
    let state = query
        .state
        .as_ref()
        .map(|state| {
            serde_json::from_value::<AgentState>(json!(state))
                .map_err(|_| ApiError::bad("invalid agent state"))
        })
        .transpose()?;
    let text = query.q.unwrap_or_default().to_lowercase();
    if text.len() > 256 {
        return Err(ApiError::bad("search text too long"));
    }
    let value = app.db(move |store| {
        let (experiments, agents) = owned_data(&store, &creator, query.experiment_id.as_deref())?;
        let names = experiments.iter().map(|e| (e.id.as_str(), e.config.name.to_lowercase())).collect::<BTreeMap<_,_>>();
        let filtered = agents.into_iter().filter(|a| {
            state.as_ref().is_none_or(|state| &a.state == state)
                && (text.is_empty() || a.id.contains(&text) || names.get(a.experiment_id.as_str()).is_some_and(|name| name.contains(&text)))
        }).collect::<Vec<_>>();
        let total = filtered.len();
        let rows = filtered.into_iter().filter(|a| query.after.is_none_or(|after| a.slot > after)).take(query.limit.unwrap_or(200).clamp(1, 1000)).collect::<Vec<_>>();
        let next_cursor = rows.last().map(|a| a.slot);
        Ok(json!({"agents": rows.iter().map(api::public_agent).collect::<Vec<_>>(), "total":total,"next_cursor":next_cursor}))
    }).await?;
    Ok(Json(value))
}

#[derive(Deserialize, Default)]
pub struct TopologyQuery {
    pub experiment_id: Option<String>,
}
pub async fn topology(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<TopologyQuery>,
) -> Result<Json<Value>, ApiError> {
    let creator = api::owner(&app, &headers)?;
    let value = app.db(move |store| {
        let (experiments, agents) = owned_data(&store, &creator, query.experiment_id.as_deref())?;
        let mut nodes = vec![
            json!({"id":"environment:llm","kind":"llm","label":"Public LLM","state":"environment"}),
            json!({"id":"environment:judge","kind":"judge","label":"External judge","state":"environment"}),
        ];
        if experiments.iter().any(|e| e.config.network.internet) {
            nodes.push(json!({"id":"environment:internet","kind":"internet","label":"Internet","state":"environment"}));
        }
        let mut edges = Vec::new();
        let configs = experiments.iter().map(|e| (e.id.clone(), &e.config)).collect::<BTreeMap<_,_>>();
        for experiment in &experiments {
            let id = format!("experiment:{}", experiment.id);
            nodes.push(json!({"id":id,"kind":"experiment","label":experiment.config.name,"experiment_id":experiment.id,"state":"configured"}));
            if experiment.config.network.peers {
                nodes.push(json!({"id":format!("peers:{}",experiment.id),"kind":"peers","label":"Experiment peer services","experiment_id":experiment.id,"state":"configured"}));
            }
        }
        for agent in &agents {
            let config = configs[&agent.experiment_id];
            nodes.push(json!({"id":agent.id,"kind":"agent","label":format!("Individual {:03}",agent.slot),"agent_id":agent.id,"experiment_id":agent.experiment_id,"state":agent.state,"available":agent.available(),"balance":agent.balance(),"generation":agent.generation}));
            let mut edge = |target: String, kind: &str, basis: &str, count: Option<u64>| {
                edges.push(json!({"id":format!("{}:{kind}:{basis}",agent.id),"source":agent.id,"target":target,"kind":kind,"basis":basis,"count":count}));
            };
            edge(format!("experiment:{}",agent.experiment_id), "membership", "configured", None);
            edge("environment:llm".into(), "llm_access", "configured", None);
            edge("environment:judge".into(), "reward_eligibility", "configured", None);
            if config.network.internet { edge("environment:internet".into(), "internet_access", "configured", None); }
            if config.network.peers { edge(format!("peers:{}",agent.experiment_id), "peer_access", "configured", None); }
            if agent.used > 0 { edge("environment:llm".into(), "settled_llm_tokens", "observed", Some(agent.used)); }
            let submissions = store.submission_counts(&agent.id)?;
            let count = submissions.values().sum::<u64>();
            if count > 0 { edge("environment:judge".into(), "submitted_artifacts", "observed", Some(count)); }
        }
        Ok(json!({"nodes":nodes,"edges":edges,"generated_at":now_ms(),"semantics":{"configured":"Allowed capabilities and experiment membership; not observed traffic.","observed":"Durable accounting or submissions; cumulative, not live network packets."}}))
    }).await?;
    Ok(Json(value))
}

pub async fn root() -> Redirect {
    Redirect::temporary("/console/")
}
pub async fn index(State(app): State<App>) -> Response {
    static_file(app, "index.html".into()).await
}
pub async fn asset(State(app): State<App>, Path(path): Path<String>) -> Response {
    static_file(app, path).await
}
async fn static_file(app: App, path: String) -> Response {
    let Some(root) = app.0.config.console_dir.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Observer console assets have not been installed.",
        )
            .into_response();
    };
    let policy = if path == "preview.html" {
        "sandbox allow-scripts allow-downloads; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; object-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'"
    } else {
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; frame-src 'self' blob:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"
    };
    let result = tokio::task::spawn_blocking(move || load_asset(root, &path)).await;
    match result {
        Ok(Ok((bytes, mime, cache))) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", mime)
            .header("cache-control", cache)
            .header("x-content-type-options", "nosniff")
            .header("referrer-policy", "no-referrer")
            .header("content-security-policy", policy)
            .body(Body::from(bytes))
            .unwrap(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => ApiError::internal("asset reader failed").into_response(),
    }
}
fn load_asset(
    root: PathBuf,
    path: &str,
) -> Result<(Vec<u8>, &'static str, &'static str), ApiError> {
    let requested = std::path::Path::new(path);
    if path.is_empty()
        || requested
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || path.contains('\\')
    {
        return Err(ApiError(StatusCode::NOT_FOUND, "asset not found".into()));
    }
    let dir =
        cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority()).map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "console assets unavailable".into(),
            )
        })?;
    let mime = match requested.extension().and_then(|v| v.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => return Err(ApiError(StatusCode::NOT_FOUND, "asset not found".into())),
    };
    let mut file = dir
        .open(path)
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "asset not found".into()))?;
    let metadata = file
        .metadata()
        .map_err(|_| ApiError::internal("asset metadata unavailable"))?;
    if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
        return Err(ApiError(StatusCode::NOT_FOUND, "asset not found".into()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|_| ApiError::internal("asset read failed"))?;
    // Assets use stable names, so caches must revalidate after deployments.
    Ok((bytes, mime, "no-cache"))
}
