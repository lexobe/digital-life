use anyhow::{Context, Result, ensure};
use clap::Parser;
use survive_server::{App, api, config::Config, lifecycle};

mod environment;

#[derive(Parser)]
#[command(version, about = "Rust autonomous-agent survival environment")]
struct Args {
    #[arg(long, env = "SURVIVE_CONFIG")]
    config: Option<std::path::PathBuf>,
    /// Private KEY=value file; defaults to /etc/survive-platform/environment, then .env.
    #[arg(long, env = "SURVIVE_ENV_FILE")]
    env_file: Option<std::path::PathBuf>,
    /// Validate configuration and credentials without starting listeners or sandboxes.
    #[arg(long)]
    check: bool,
    /// Export only the sandbox table as TOML; does not open state or credentials.
    #[arg(long)]
    export_sandbox: bool,
    /// Finalize prepared desktop limits for one stopped individual while the service is offline.
    #[arg(long, value_name = "ID", conflicts_with_all = ["check", "export_sandbox"])]
    finish_desktop_upgrade: Option<String>,
}
fn main() -> Result<()> {
    let mut args = Args::parse();
    // Rust 2024 requires environment writes to happen before Tokio or tracing
    // can create other threads. Explicit process environment takes precedence.
    if let Some(path) = environment::select(args.env_file.as_deref())? {
        for (key, value) in environment::read(&path)? {
            if std::env::var_os(&key).is_none() {
                // SAFETY: synchronous program entry; no threads have been started.
                unsafe { std::env::set_var(key, value) };
            }
        }
    }
    if args.config.is_none() {
        args.config = Some(
            std::env::var_os("SURVIVE_CONFIG")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "config.local.toml".into()),
        );
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    let config: Config = toml::from_str(&std::fs::read_to_string(args.config.unwrap())?)?;
    if args.export_sandbox {
        let sandbox = config
            .sandbox
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sandbox configuration missing"))?;
        print!("{}", toml::to_string(sandbox)?);
        return Ok(());
    }
    if let Some(id) = args.finish_desktop_upgrade {
        let sandbox = config.sandbox.context("sandbox configuration missing")?;
        ensure!(
            sandbox.data_dir == config.data_dir,
            "sandbox.data_dir must equal data_dir"
        );
        let manager = survive_sandbox::SandboxManager::new(sandbox)?;
        let path = config.data_dir.join("platform.redb");
        ensure!(path.is_file(), "existing platform database required");
        let store = survive_core::Store::open(path)
            .context("open database exclusively for offline upgrade")?;
        let state = manager
            .with_stopped_spec(&id, move |spec| {
                store.upgrade_desktop_limits(
                    &spec.agent_id,
                    &spec.experiment_id,
                    spec.slot,
                    &survive_core::token_hash(&spec.token),
                    &spec.network,
                    &spec.limits,
                )
            })
            .await?;
        tracing::info!(organism = %id, ?state, "desktop upgrade finalized");
        return Ok(());
    }
    let app = App::new(config)?;
    if args.check {
        tracing::info!("configuration validated");
        return Ok(());
    }
    lifecycle::recover(&app)
        .await
        .map_err(|e| anyhow::anyhow!(e.1))?;
    let owner = tokio::net::TcpListener::bind(app.0.config.listen).await?;
    let environment = bind_environment(app.0.config.env_listen).await?;
    tracing::info!(listen=%app.0.config.listen,environment=%app.0.config.env_listen,test_mode=app.0.config.test_mode,"survival platform started");
    let life = tokio::spawn(lifecycle::run(app.clone()));
    let accounting = app.0.gateway.clone();
    let reconciliation = tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(30));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            match accounting.reconcile_unknown().await {
                Ok(count) if count > 0 => {
                    tracing::info!(count, "reconciled previously unknown usage")
                }
                Err(error) => tracing::warn!(error=?error,"usage reconciliation deferred"),
                _ => {}
            }
        }
    });
    let owner_app = api::owner_router(app.clone());
    let env_app = api::environment_router(app);
    let a = tokio::spawn(async move {
        axum::serve(
            owner,
            owner_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown())
        .await
    });
    let b = tokio::spawn(async move {
        axum::serve(
            environment,
            env_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown())
        .await
    });
    tokio::select! {result=a=>{result??;},result=b=>{result??;}}
    life.abort();
    reconciliation.abort();
    Ok(())
}

async fn bind_environment(
    address: std::net::SocketAddr,
) -> std::io::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => Ok(listener),
        #[cfg(target_os = "linux")]
        Err(error) if error.kind() == std::io::ErrorKind::AddrNotAvailable && address.is_ipv4() => {
            use std::os::fd::AsRawFd;
            // A control-only installation may not have provisioned its private
            // guest network yet. Bind that exact private address, never 0.0.0.0.
            let socket = tokio::net::TcpSocket::new_v4()?;
            let enabled: libc::c_int = 1;
            // SAFETY: socket owns a valid fd; the option points to an initialized
            // integer whose lifetime covers setsockopt and length matches it.
            let result = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::IPPROTO_IP,
                    libc::IP_FREEBIND,
                    &enabled as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&enabled) as libc::socklen_t,
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            }
            socket.bind(address)?;
            tracing::warn!(%address, "environment listener waiting for private guest network");
            socket.listen(128)
        }
        Err(error) => Err(error),
    }
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
