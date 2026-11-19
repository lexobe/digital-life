use crate::{ApiError, App};
use anyhow::{Context, Result};
use std::path::Path;
use survive_core::*;
use survive_sandbox::{SandboxSpec, SandboxStatus};

pub async fn seed_organism(
    app: &App,
    agent: &Agent,
    profile: &OrganismProfile,
) -> Result<(), ApiError> {
    let root = app.0.config.data_dir.join("agents").join(&agent.id);
    let dna = profile.birth.dna.clone();
    let default_model = profile.birth.default_model.clone();
    let source = app.0.config.source_dir.clone();
    let environment_id = profile.environment_id.clone();
    let id = agent.id.clone();
    let (description, services) = app
        .db(move |store| {
            Ok((
                store.environment(&environment_id)?.description,
                store.services(&id)?,
            ))
        })
        .await?;
    let name = if profile.name.is_empty() {
        format!("生物 {}", agent.slot)
    } else {
        profile.name.clone()
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        // Assets live on a host-owned, read-only guest mount and survive custom boot programs.
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets)?;
        write_asset(&assets, "environment.md", description.as_bytes())?;
        let home = desktop_home(&name, &description, &services);
        write_asset(&assets, "home.html", home.as_bytes())?;
        if root.join("seeded").exists() {
            return Ok(());
        }
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        for (file, text) in [("dna.md", dna), ("model.txt", default_model)] {
            if !work.join(file).exists() {
                std::fs::write(work.join(file), text)?;
            }
        }
        if !work.join("boot.json").exists() {
            std::fs::write(
                work.join("boot.json"),
                r#"{"argv":["/opt/survive/survive-agent"]}"#,
            )?;
        }
        if let Some(source) = source {
            let destination = work.join("source");
            std::fs::create_dir_all(&destination)?;
            copy_tree(
                &source.join("crates/agent"),
                &destination.join("crates/agent"),
            )?;
            copy_tree(
                &source.join("crates/core"),
                &destination.join("crates/core"),
            )?;
            std::fs::copy(
                source.join("seed/Cargo.toml"),
                destination.join("Cargo.toml"),
            )?;
            std::fs::copy(
                source.join("seed/Cargo.lock"),
                destination.join("Cargo.lock"),
            )?;
        }
        std::fs::write(root.join("seeded"), "1\n")?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(ApiError::from)
}
fn write_asset(parent: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let path = parent.join(name);
    if std::fs::read(&path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o444))?;
    }
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn desktop_home(name: &str, environment: &str, services: &[ServiceRegistration]) -> String {
    fn text(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    let mut links = String::new();
    if !services.is_empty() {
        links.push_str("<h2>已登记的本地服务</h2><ul>");
        for s in services {
            links.push_str(&format!(
                "<li><a href=\"http://127.0.0.1:{}/\">{}</a></li>",
                s.port,
                text(&s.name)
            ));
        }
        links.push_str("</ul>");
    }
    let (prefix, rest) = include_str!("desktop-home.html")
        .split_once("{name}")
        .expect("home title placeholder");
    let (before_env, rest) = rest
        .split_once("{description}")
        .expect("home description placeholder");
    let (before_services, end) = rest
        .split_once("{services}")
        .expect("home services placeholder");
    [
        prefix,
        &text(name),
        before_env,
        &text(environment),
        before_services,
        &links,
        end,
    ]
    .concat()
}
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("missing seed sources {}", source.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let ty = entry.file_type()?;
        if ty.is_symlink() || name == "target" || name == ".git" {
            continue;
        }
        if ty.is_dir() {
            copy_tree(&entry.path(), &destination.join(name))?;
        } else {
            std::fs::copy(entry.path(), destination.join(name))?;
        }
    }
    Ok(())
}

pub async fn recover(app: &App) -> Result<(), ApiError> {
    // In-flight requests were not durably settled before shutdown; never silently refund.
    let rs = app.db(|s| s.unsettled_reservations()).await?;
    for r in rs
        .into_iter()
        .filter(|r| r.status == ReservationStatus::Reserved)
    {
        app.db(move |s| s.mark_unknown(&r.id, r.upstream_id))
            .await?;
    }
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap_or_else(|_| "non-linux".into());
    let path = app.0.config.data_dir.join("host-boot-id");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    if old != boot {
        let agents = app
            .db(|s| {
                s.profiles(None)?
                    .into_iter()
                    .map(|p| s.get_agent(&p.id))
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
        for a in agents
            .into_iter()
            .filter(|a| matches!(a.state, AgentState::Running | AgentState::Starting))
        {
            app.db(move |s| {
                s.set_agent_state(
                    &a.id,
                    AgentState::Queued,
                    Some("recovering after host restart".into()),
                )
            })
            .await?;
        }
        tokio::fs::write(path, boot)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
    }
    Ok(())
}

pub async fn run(app: App) {
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        if let Err(e) = tick(&app).await {
            tracing::error!(error=%e.1,"lifecycle reconciliation failed");
        }
    }
}
/// Cache runtime health so UI reads do not shell out or block on host checks.
/// Unavailable host isolation keeps the control plane available without
/// replacing the isolated workload with a host process.
pub async fn refresh_runtime(app: &App) -> crate::console::RuntimeReadiness {
    let Some(sandbox) = &app.0.sandbox else {
        return app.0.runtime.read().await.clone();
    };
    let current = app.0.runtime.read().await.clone();
    if current
        .checked_at
        .is_some_and(|at| now_ms().saturating_sub(at) < 30_000)
    {
        return current;
    }
    let checked =
        match tokio::time::timeout(std::time::Duration::from_secs(10), sandbox.doctor()).await {
            Ok(result) => crate::console::RuntimeReadiness::checked(result),
            Err(_) => crate::console::RuntimeReadiness::checked(Err(anyhow::anyhow!(
                "runtime preflight timed out"
            ))),
        };
    *app.0.runtime.write().await = checked.clone();
    checked
}
pub async fn tick(app: &App) -> Result<(), ApiError> {
    let _guard = app.0.lifecycle_lock.lock().await;
    let Some(sandbox) = &app.0.sandbox else {
        return Ok(());
    };
    let profiles = app.db(|s| s.profiles(None)).await?;
    // Check projected deadlines even if the runtime or inventory doctor is unhealthy.
    let mut funded = Vec::new();
    let mut revocation_error = None;
    for p in profiles {
        let id = p.id.clone();
        let life = app.db(move |s| s.life_state(&id)).await?;
        if life.stop_reasons.is_empty() {
            funded.push(p);
            continue;
        }
        if let Err(error) = stop_if_depleted(app, &p.id).await {
            revocation_error = Some(error);
        }
    }
    if let Some(error) = revocation_error {
        return Err(error);
    }
    if funded.is_empty() {
        refresh_runtime(app).await;
        return Ok(());
    }
    let statuses = sandbox.statuses().await?;
    let mut memory = 0u64;
    let mut observed = Vec::new();
    for p in funded {
        let status = statuses
            .get(&p.id)
            .copied()
            .unwrap_or(SandboxStatus::Missing);
        let running = status == SandboxStatus::Running;
        let life = checkpoint(app, &p.id, running).await?;
        if !life.stop_reasons.is_empty() {
            stop_if_depleted(app, &p.id).await?;
            continue;
        }
        if running {
            memory = memory.saturating_add(sandbox.memory_reservation(p.effective_limits()));
        }
        let id = p.id.clone();
        let a = app.db(move |s| s.get_agent(&id)).await?;
        observed.push((a, p, status));
    }
    if !refresh_runtime(app).await.available {
        return Ok(());
    }
    let mut started = 0;
    for (agent, profile, status) in observed {
        if matches!(agent.state, AgentState::Queued | AgentState::Starting) {
            if status == SandboxStatus::Running {
                let id = agent.id;
                app.db(move |s| s.set_agent_state(&id, AgentState::Running, None))
                    .await?;
                continue;
            }
            if started >= 2
                || agent.available() == 0
                || memory.saturating_add(sandbox.memory_reservation(profile.effective_limits()))
                    > app.0.config.sandbox.as_ref().unwrap().total_memory_bytes
            {
                continue;
            }
            if let Err(error) = seed_organism(app, &agent, &profile).await {
                let id = agent.id.clone();
                app.db(move |s| s.set_agent_state(&id, AgentState::Failed, Some(error.1)))
                    .await?;
                continue;
            }
            let id = agent.id.clone();
            let token = app.db(move |s| s.agent_credential(&id)).await?;
            let spec = SandboxSpec {
                agent_id: agent.id.clone(),
                experiment_id: profile.environment_id.clone(),
                slot: agent.slot,
                token,
                limits: profile.effective_limits().clone(),
                network: profile.birth.network.clone(),
            };
            let id = agent.id.clone();
            app.db(move |s| s.set_agent_state(&id, AgentState::Starting, None))
                .await?;
            let result = async {
                sandbox.prepare(&spec).await?;
                sandbox.start(&spec.agent_id).await
            }
            .await;
            let id = agent.id.clone();
            match result {
                Ok(()) => {
                    checkpoint(app, &id, true).await?;
                    app.db(move |s| s.set_agent_state(&id, AgentState::Running, None))
                        .await?;
                    memory += sandbox.memory_reservation(profile.effective_limits());
                    started += 1;
                }
                Err(error) => {
                    tracing::error!(agent_id=%id,error=%error,"sandbox start failed");
                    app.db(move |s| {
                        s.set_agent_state(&id, AgentState::Failed, Some(error.to_string()))
                    })
                    .await?;
                }
            }
        } else if agent.state == AgentState::Running && status != SandboxStatus::Running {
            let id = agent.id;
            app.db(move |s| {
                s.set_agent_state(
                    &id,
                    AgentState::Failed,
                    Some("sandbox stopped; no automatic code rollback or restart".into()),
                )
            })
            .await?;
        }
    }
    Ok(())
}

pub(crate) async fn checkpoint(app: &App, id: &str, running: bool) -> Result<LifeState, ApiError> {
    let id = id.to_owned();
    app.db(move |s| {
        let (boot, mono) = runtime_clock()?;
        s.observe_runtime(&id, running, &boot, mono, now_ms())
    })
    .await
}
async fn stop_if_depleted(app: &App, id: &str) -> Result<(), ApiError> {
    let Some(sandbox) = &app.0.sandbox else {
        return Ok(());
    };
    let key = id.to_owned();
    let life = app.db(move |s| s.life_state(&key)).await?;
    if life.stop_reasons.is_empty() {
        return Ok(());
    }
    if sandbox
        .status(id)
        .await
        .is_ok_and(|v| v == SandboxStatus::Running)
    {
        checkpoint(app, id, true).await?;
    }
    sandbox.stop(id).await?;
    checkpoint(app, id, false).await?;
    let key = id.to_owned();
    app.db(move |s| s.confirm_resource_stop(&key)).await?;
    Ok(())
}
