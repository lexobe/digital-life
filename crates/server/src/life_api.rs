use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(super) fn owner_routes() -> Router<App> {
    Router::new()
        .route("/api/v2/overview", get(overview))
        .route("/api/v2/topology", get(topology))
        .route(
            "/api/v2/environments",
            get(environments).post(create_environment),
        )
        .route("/api/v2/environments/{id}", get(environment))
        .route(
            "/api/v2/environments/{id}/organisms",
            get(list_members).post(birth),
        )
        .route("/api/v2/organisms", get(list_all))
        .route("/api/v2/organisms/{id}", get(organism))
        .route("/api/v2/organisms/{id}/grants", get(grants).post(grant))
        .route("/api/v2/organisms/{id}/reports", get(reports))
        .route(
            "/api/v2/organisms/{id}/messages",
            get(messages).post(send_creator_message),
        )
        .route(
            "/api/v2/organisms/{id}/reports/{report}",
            get(report_detail),
        )
        .route("/api/v2/organisms/{id}/events", get(events))
        .route("/api/v2/organisms/{id}/events/{event}", get(event_detail))
        .route("/api/v2/organisms/{id}/files", get(files))
        .route("/api/v2/organisms/{id}/ledger", get(organism_ledger))
        .route("/api/v2/organisms/{id}/services", get(services))
}
pub(super) fn guest_routes() -> Router<App> {
    Router::new()
        .route("/env/v2/self", get(guest_self))
        .route("/env/v2/life", get(guest_life))
        .route("/env/v2/name", post(rename))
        .route("/env/v2/reports", post(send_report).get(guest_reports))
        .route(
            "/env/v2/messages",
            get(guest_messages).post(reply_to_message),
        )
        .route("/env/v2/models", get(guest_models))
        .route("/env/v2/events", get(self_events))
        .route("/env/v2/events/{id}", get(self_event_detail))
        .route("/env/v2/ledger", get(guest_ledger))
        .route("/env/v2/peers", get(guest_peers))
        .route("/env/v2/services", post(guest_service))
        .route("/env/v2/restart", post(restart))
}
fn projection(a: &Agent, p: &OrganismProfile, l: &LifeState) -> Value {
    let mut v = json!(p.birth);
    let obj = v.as_object_mut().expect("birth traits are an object");
    obj.extend(json!(l).as_object().unwrap().clone());
    obj.insert("runtime_limits".into(), json!(p.effective_limits()));
    obj.extend(json!({"id":a.id,"environment_id":p.environment_id,"owner":p.owner,"name":p.name,
        "created_at":p.created_at,"slot":a.slot,"generation":a.generation,"state":a.state,"last_error":a.last_error,
        "runtime_running":p.running_since_ms.is_some(),"life":l,"granted":a.granted,"used":a.used}).as_object().unwrap().clone());
    v
}
fn owner_profile(store: &Store, who: &str, id: &str) -> anyhow::Result<OrganismProfile> {
    let p = store.organism_profile(id)?;
    anyhow::ensure!(p.owner == who, "not_found: organism");
    Ok(p)
}
fn owner_environment(store: &Store, who: &str, id: &str) -> anyhow::Result<LifeEnvironment> {
    let env = store.environment(id)?;
    anyhow::ensure!(env.owner == who, "not_found: environment");
    Ok(env)
}
fn one(store: &Store, p: &OrganismProfile) -> anyhow::Result<Value> {
    Ok(projection(
        &store.get_agent(&p.id)?,
        p,
        &store.life_state(&p.id)?,
    ))
}
async fn environments(State(app): State<App>, h: HeaderMap) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    Ok(Json(
        json!({"environments":app.db(move|s|s.environments(&who)).await?}),
    ))
}
async fn create_environment(
    State(app): State<App>,
    h: HeaderMap,
    Json(input): Json<EnvironmentInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let who = owner(&app, &h)?;
    if let Some(ids) = &input.allowed_models {
        let catalog = app.0.gateway.catalog(None);
        if ids
            .iter()
            .any(|id| !catalog.iter().any(|row| row["id"].as_str() == Some(id)))
        {
            return Err(ApiError::bad("unknown environment model ID"));
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"environment":app.db(move|s|s.create_environment(&who,input)).await?})),
    ))
}
async fn environment(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    Ok(Json(
        json!({"environment":app.db(move|s|owner_environment(&s,&who,&id)).await?}),
    ))
}
async fn organism(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    Ok(Json(
        json!({"organism":app.db(move|s|one(&s,&owner_profile(&s,&who,&id)?)).await?}),
    ))
}
async fn members(app: App, h: HeaderMap, env: Option<String>) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let items = app
        .db(move |s| {
            if let Some(id) = &env {
                owner_environment(&s, &who, id)?;
            }
            s.profiles(Some(&who))?
                .iter()
                .filter(|p| env.as_ref().is_none_or(|id| p.environment_id == *id))
                .map(|p| one(&s, p))
                .collect::<anyhow::Result<Vec<Value>>>()
        })
        .await?;
    Ok(Json(json!({"organisms":items})))
}
async fn list_all(State(app): State<App>, h: HeaderMap) -> Result<Json<Value>, ApiError> {
    members(app, h, None).await
}
async fn list_members(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    members(app, h, Some(id)).await
}
fn preferred_model(catalog: &[Value], allowed: Option<&[String]>) -> Result<String, ApiError> {
    let mut choices = catalog.iter().filter(|row| {
        allowed.is_none_or(|ids| ids.iter().any(|id| row["id"].as_str() == Some(id)))
    });
    choices
        .clone()
        .find(|row| row["available"] == true)
        .or_else(|| choices.next())
        .and_then(|row| row["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| ApiError::bad("no configured model matches the permitted model IDs"))
}

async fn birth(
    State(app): State<App>,
    h: HeaderMap,
    Path(env): Path<String>,
    Json(payload): Json<Value>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let who = owner(&app, &h)?;
    let habitat = app
        .db({
            let (who, env) = (who.clone(), env.clone());
            move |s| owner_environment(&s, &who, &env)
        })
        .await?;
    let catalog = app.0.gateway.catalog(habitat.allowed_models.as_deref());
    let infer_default = payload.pointer("/defaults/default_model").is_none();
    let mut batch: BirthBatch = serde_json::from_value(payload)
        .map_err(|error| ApiError::bad(format!("invalid birth parameters: {error}")))?;
    if infer_default {
        batch.defaults.default_model =
            preferred_model(&catalog, batch.defaults.allowed_models.as_deref())?;
        for slot in &mut batch.overrides {
            if slot.default_model.is_none()
                && let Some(ids) = &slot.allowed_models
                && !ids.contains(&batch.defaults.default_model)
            {
                slot.default_model = Some(preferred_model(&catalog, Some(ids))?);
            }
        }
    }
    let models: BTreeSet<_> = catalog.iter().filter_map(|r| r["id"].as_str()).collect();
    let members = batch.resolve()?;
    for traits in std::iter::once(&batch.defaults).chain(&members) {
        if !models.contains(traits.default_model.as_str())
            || traits
                .allowed_models
                .as_ref()
                .is_some_and(|x| x.iter().any(|m| !models.contains(m.as_str())))
        {
            return Err(ApiError::bad("model ID unavailable in this environment"));
        }
    }
    for traits in &members {
        if let Some(sandbox) = &app.0.sandbox
            && sandbox.memory_reservation(&traits.limits) > sandbox.config().total_memory_bytes
        {
            return Err(ApiError::bad("individual memory limit exceeds pool"));
        }
    }
    let organisms = app
        .db(move |s| {
            s.birth_organisms(&who, &env, batch)?
                .iter()
                .map(|a| {
                    let p = s.organism_profile(&a.id)?;
                    Ok((
                        a.clone(),
                        p.clone(),
                        projection(a, &p, &s.life_state(&a.id)?),
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .await?;
    if app.0.config.test_mode {
        for (a, p, _) in &organisms {
            crate::lifecycle::seed_organism(&app, a, p).await?;
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"organisms":organisms.into_iter().map(|(_,_,v)|v).collect::<Vec<_>>()})),
    ))
}
async fn grant(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<GrantInput>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let v = app
        .db(move |s| {
            let g = s.grant(&who, &id, input)?;
            Ok(json!({"grant":g,"life":s.life_state(&id)?}))
        })
        .await?;
    Ok(Json(v))
}
async fn grants(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let value = app
        .db(move |s| {
            owner_profile(&s, &who, &id)?;
            let (grants, cursor) = s.grant_page(
                &id,
                page.after.as_deref(),
                page.limit.unwrap_or(100).clamp(1, 200),
            )?;
            Ok(json!({"next_cursor":cursor,"grants":grants}))
        })
        .await?;
    Ok(Json(value))
}
async fn reports(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let v = app
        .db(move |s| {
            owner_profile(&s, &who, &id)?;
            let (rows, cursor) = s.reports(
                &id,
                page.after.as_deref(),
                page.limit.unwrap_or(20).clamp(1, 100),
            )?;
            let rows: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    let mut v = json!(r);
                    v.as_object_mut().unwrap().remove("snapshots");
                    v
                })
                .collect();
            Ok(json!({"reports":rows,"next_cursor":cursor}))
        })
        .await?;
    Ok(Json(v))
}
async fn guest_reports(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let v = app
        .db(move |s| {
            let (rows, next) = s.reports(
                &a.id,
                page.after.as_deref(),
                page.limit.unwrap_or(20).clamp(1, 20),
            )?;
            let rows: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    let mut v = json!(r);
                    v.as_object_mut().unwrap().remove("snapshots");
                    v
                })
                .collect();
            Ok(json!({"reports":rows,"next_cursor":next}))
        })
        .await?;
    Ok(Json(v))
}
async fn report_detail(
    State(app): State<App>,
    h: HeaderMap,
    Path((id, rid)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let r = app
        .db(move |s| {
            owner_profile(&s, &who, &id)?;
            let r = s.report(&rid)?;
            anyhow::ensure!(r.organism_id == id, "not_found: report");
            Ok(r)
        })
        .await?;
    Ok(Json(json!({"report":r})))
}
fn dashboard_snapshot(store: &Store, creator: String) -> anyhow::Result<Value> {
    let environments = store.environments(&creator)?;
    let mut organisms = Vec::new();
    let mut recent = Vec::new();
    let mut running = 0;
    let mut available_energy = 0u64;
    for profile in store.profiles(Some(&creator))? {
        let agent = store.get_agent(&profile.id)?;
        let life = store.life_state(&profile.id)?;
        running += usize::from(agent.state == AgentState::Running);
        available_energy = available_energy.saturating_add(life.energy_available);
        organisms.push(projection(&agent, &profile, &life));
        recent.extend(store.recent_events(&profile.id, 20)?);
        recent.sort_by(|a, b| b.id.cmp(&a.id));
        recent.truncate(20);
    }
    organisms.sort_by_key(|a| a["slot"].as_u64());
    Ok(json!({
        "creator_id": creator,
        "generated_at": now_ms(),
        "summary": {
            "running": running,
            "environments": environments.len(),
            "organisms": organisms.len(),
            "energy_available": available_energy
        },
        "environments": environments,
        "organisms": organisms,
        "recent_events": recent
    }))
}
async fn overview(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let creator = owner(&app, &headers)?;
    let mut value = app
        .db(move |store| dashboard_snapshot(&store, creator))
        .await?;
    value["system"] = crate::console::system(&app).await;
    value["system"]["models"] = json!(app.0.gateway.catalog(None));
    Ok(Json(value))
}

fn topology_snapshot(store: &Store, creator: &str) -> anyhow::Result<Value> {
    let mut nodes = vec![
        json!({"id": "llm", "kind": "llm", "label": "公共模型"}),
        json!({"id": "creator", "kind": "creator", "label": "创造者"}),
        json!({"id": "internet", "kind": "internet", "label": "互联网"}),
    ];
    let mut edges = Vec::new();
    let mut add = |source: &str, target: &str, kind: &str, basis: &str, count: Option<u64>| {
        edges.push(json!({
            "id": format!("{source}:{kind}:{target}"),
            "source": source, "target": target, "kind": kind,
            "basis": basis, "count": count
        }));
    };
    for env in store.environments(creator)? {
        nodes.push(
            json!({"id":env.id,"environment_id":env.id,"kind":"environment","label":env.name}),
        );
        nodes.push(json!({"id":format!("peers:{}", env.id),"kind":"peers","environment_id":env.id,"label":"同环境服务"}));
    }
    for profile in store.profiles(Some(creator))? {
        let agent = store.get_agent(&profile.id)?;
        let id = &agent.id;
        let label = if profile.name.is_empty() {
            id
        } else {
            &profile.name
        };
        nodes.push(json!({"id":id,"organism_id":id,"environment_id":profile.environment_id,"kind":"organism","state":agent.state,"label":label}));
        add(
            id,
            &profile.environment_id,
            "membership",
            "configured",
            None,
        );
        add(id, "llm", "llm_access", "configured", None);
        add(id, "creator", "report_channel", "configured", None);
        if profile.birth.network.internet {
            add(id, "internet", "internet_access", "configured", None);
        }
        if profile.birth.network.peers {
            add(
                id,
                &format!("peers:{}", profile.environment_id),
                "peer_access",
                "configured",
                None,
            );
        }
        if agent.used > 0 {
            add(id, "llm", "settled_energy", "observed", Some(agent.used));
        }
        let (reports, grants) = store.activity_counts(id)?;
        if reports > 0 {
            add(id, "creator", "reported", "observed", Some(reports));
        }
        if grants > 0 {
            add("creator", id, "granted", "observed", Some(grants));
        }
    }
    Ok(json!({
        "nodes": nodes, "edges": edges, "generated_at": now_ms(),
        "semantics": {"configured":"permissions and membership","observed":"durable recorded activity"}
    }))
}
async fn topology(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let creator = owner(&app, &headers)?;
    Ok(Json(
        app.db(move |store| topology_snapshot(&store, &creator))
            .await?,
    ))
}
async fn guest_self(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let (p, l, e, (pending, recent)) = app
        .db({
            let id = a.id.clone();
            move |s| {
                let p = s.organism_profile(&id)?;
                let e = s.environment(&p.environment_id)?;
                Ok((p, s.life_state(&id)?, e, s.message_context(&id)?))
            }
        })
        .await?;
    Ok(Json(
        json!({"organism":projection(&a,&p,&l),"life":l,"environment":e,
            "pending_messages":pending,"recent_messages":recent,
            "minimum_input_reservation":app.0.config.gateway.minimum_input_reservation,"models":app.0.gateway.organism_catalog(&e, &p.birth),"paths":{"environment":"/opt/survive/environment.md","dna":"/work/dna.md","model":"/work/model.txt","source":"/work/source","boot":"/work/boot.json"}}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageQuery {
    after: Option<u64>,
    limit: Option<usize>,
    #[serde(default)]
    pending: bool,
}

impl MessageQuery {
    fn page(&self, store: &Store, id: &str) -> anyhow::Result<MessagePage> {
        let default_limit = if self.pending { 1 } else { 50 };
        store.messages(
            id,
            self.after,
            self.limit.unwrap_or(default_limit),
            self.pending,
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatorMessageInput {
    text: String,
    idempotency_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyInput {
    text: String,
    reply_to: u64,
    idempotency_key: String,
}

async fn messages(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<MessageQuery>,
) -> Result<Json<MessagePage>, ApiError> {
    let who = owner(&app, &h)?;
    Ok(Json(
        app.db(move |s| {
            owner_profile(&s, &who, &id)?;
            query.page(&s, &id)
        })
        .await?,
    ))
}

async fn guest_messages(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<MessageQuery>,
) -> Result<Json<MessagePage>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    Ok(Json(app.db(move |s| query.page(&s, &a.id)).await?))
}

async fn send_creator_message(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<CreatorMessageInput>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    let message = app
        .db(move |s| s.send_creator_message(&who, &id, input.text, &input.idempotency_key))
        .await?;
    Ok(Json(json!({ "message": message })))
}

async fn reply_to_message(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(input): Json<ReplyInput>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let message = app
        .db(move |s| s.reply_to_message(&a.id, input.reply_to, input.text, &input.idempotency_key))
        .await?;
    Ok(Json(json!({ "message": message })))
}
async fn guest_life(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    Ok(Json(json!(app.db(move |s| s.life_state(&a.id)).await?)))
}
async fn guest_models(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    Ok(Json(json!({"models":organism_models(&app, a.id).await?})))
}
#[derive(Deserialize)]
struct Name {
    name: String,
}
async fn rename(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(input): Json<Name>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let p = app
        .db(move |s| s.rename_organism(&a.id, &input.name))
        .await?;
    Ok(Json(json!({"name":p.name})))
}
async fn organism_ledger(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let who = owner(&app, &h)?;
    app.db({
        let id = id.clone();
        move |s| owner_profile(&s, &who, &id)
    })
    .await?;
    meter(&app, id, page).await
}
async fn guest_ledger(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    meter(&app, a.id, page).await
}
async fn meter(app: &App, id: String, page: Page) -> Result<Json<Value>, ApiError> {
    let mut v = ledger_value(app, id.clone(), page).await?.0;
    v["life"] = json!(app.db(move |s| s.life_state(&id)).await?);
    Ok(Json(v))
}
async fn guest_peers(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let rows = app
        .db(move |s| {
            let p = s.organism_profile(&a.id)?;
            anyhow::ensure!(p.birth.network.peers, "forbidden");
            let mut peers = Vec::new();
            for other in s.list_agents(Some(&p.environment_id))? {
                if other.id == a.id || other.state != AgentState::Running {
                    continue;
                }
                let profile = s.organism_profile(&other.id)?;
                if profile.birth.network.peers && profile.birth.network.services {
                    peers.push((other.clone(), s.services(&other.id)?));
                }
            }
            Ok(peers)
        })
        .await?;
    let mut out = Vec::new();
    for (a, services) in rows {
        let mut published = Vec::new();
        for service in services {
            let url = if let Some(sandbox) = &app.0.sandbox {
                sandbox.service_endpoint(&a.id, service.port)?
            } else {
                format!("http://{}:{}/", a.source_ip, service.port)
            };
            published.push(json!({"name":service.name,"port":service.port,"url":url}));
        }
        out.push(json!({"id":a.id,"services":published}));
    }
    Ok(Json(json!({"peers":out})))
}
async fn guest_service(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(service): Json<ServiceRegistration>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    let port = service.port;
    let id = a.id.clone();
    let url = format!("/services/{id}/{port}/");
    app.db(move |s| {
        anyhow::ensure!(s.organism_profile(&id)?.birth.network.services, "forbidden");
        s.register_service(&id, service)
    })
    .await?;
    preview::capture_registered_root(&app, &a, port).await;
    Ok(Json(json!({"path":url})))
}
async fn send_report(
    State(app): State<App>,
    h: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(input): Json<ReportInput>,
) -> Result<Json<Value>, ApiError> {
    let a = environment_agent(&app, &h, peer).await?;
    if let Some(r) = app
        .db({
            let id = a.id.clone();
            let input = input.clone();
            move |s| s.existing_report(&id, &input)
        })
        .await?
    {
        return Ok(Json(json!({"report":r})));
    }
    let root = app
        .0
        .config
        .data_dir
        .join("agents")
        .join(&a.id)
        .join("work");
    let paths = input.files.clone();
    let files = tokio::task::spawn_blocking(move || snapshot_files(root, &paths))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    let r = app.db(move |s| s.save_report(&a.id, input, files)).await?;
    Ok(Json(json!({"report":r})))
}
fn snapshot_files(root: PathBuf, paths: &[String]) -> Result<Value, ApiError> {
    if paths.len() > 16 {
        return Err(ApiError::bad("at most 16 report files"));
    }
    let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .map_err(|e| ApiError::bad(e.to_string()))?;
    let mut remaining = 128 * 1024usize;
    let mut files = Vec::new();
    for path in paths {
        if std::path::Path::new(path)
            .components()
            .any(|v| !matches!(v, std::path::Component::Normal(_)))
        {
            return Err(ApiError::bad("invalid report file path"));
        }
        let mut bytes = vec![];
        open_regular(&dir, path)?
            .take(remaining as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| ApiError::bad(e.to_string()))?;
        remaining = remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| ApiError::bad("report attachments exceed 128KiB"))?;
        files.push(json!({"path":path,"size":bytes.len(),"encoding":"base64","sha256":hex::encode(Sha256::digest(&bytes)),"data":base64::engine::general_purpose::STANDARD.encode(bytes)}));
    }
    Ok(json!(files))
}
