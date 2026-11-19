pub mod api;
pub mod config;
pub mod console;
pub mod lifecycle;

use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use config::Config;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use survive_core::Store;
use survive_gateway::Gateway;
use survive_sandbox::SandboxManager;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct App(pub Arc<Inner>);
pub struct Inner {
    pub config: Config,
    pub store: Store,
    pub gateway: Gateway,
    pub sandbox: Option<SandboxManager>,
    pub service_client: reqwest::Client,
    pub creators: BTreeMap<String, String>,
    pub lifecycle_lock: Mutex<()>,
    pub runtime: tokio::sync::RwLock<console::RuntimeReadiness>,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        ensure!(
            !config.creators.is_empty(),
            "at least one creator credential is required"
        );
        if config.test_mode {
            ensure!(
                config.listen.ip().is_loopback()
                    && config.env_listen.ip().is_loopback()
                    && config.sandbox.is_none(),
                "test_mode requires loopback listeners and no sandbox"
            );
        } else {
            ensure!(
                config.sandbox.is_some(),
                "production requires sandbox configuration"
            );
            let source = config
                .source_dir
                .as_ref()
                .context("production requires source_dir for self-editable agents")?;
            for file in [
                "seed/Cargo.toml",
                "seed/Cargo.lock",
                "crates/agent/src/main.rs",
                "crates/core/src/models.rs",
            ] {
                ensure!(
                    source.join(file).is_file(),
                    "missing self-editable seed source: {file}"
                );
            }
        }
        if let Some(s) = &config.sandbox {
            ensure!(
                s.data_dir == config.data_dir,
                "sandbox.data_dir must equal data_dir"
            );
        }
        std::fs::create_dir_all(&config.data_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut creators = BTreeMap::new();
        for c in &config.creators {
            let token = std::env::var(&c.token_env)
                .with_context(|| format!("missing creator credential env {}", c.token_env))?;
            ensure!(
                token.len() >= 32,
                "creator tokens must have at least 32 characters"
            );
            creators.insert(c.id.clone(), survive_core::token_hash(&token));
        }
        let store = Store::open(config.data_dir.join("platform.redb"))?;
        let gateway = Gateway::new(config.gateway.clone(), store.clone())?;
        let sandbox = config
            .sandbox
            .clone()
            .map(SandboxManager::new)
            .transpose()?;
        let mut service_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(3))
            .timeout(std::time::Duration::from_secs(60));
        if !config.test_mode {
            service_client =
                service_client.local_address("10.203.0.1".parse::<std::net::IpAddr>()?);
        }
        let service_client = service_client.build()?;
        let runtime = console::RuntimeReadiness::initial(config.test_mode);
        Ok(Self(Arc::new(Inner {
            config,
            store,
            gateway,
            sandbox,
            service_client,
            creators,
            lifecycle_lock: Mutex::new(()),
            runtime: tokio::sync::RwLock::new(runtime),
        })))
    }
    pub async fn db<T: Send + 'static>(
        &self,
        f: impl FnOnce(Store) -> Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        let store = self.0.store.clone();
        tokio::task::spawn_blocking(move || f(store))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(ApiError::from)
    }
}

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);
impl ApiError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    pub fn forbidden() -> Self {
        Self(StatusCode::FORBIDDEN, "forbidden".into())
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, message.into())
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let message = e.to_string();
        let status = if message == "forbidden" {
            StatusCode::FORBIDDEN
        } else if message.starts_with("not_found:") {
            StatusCode::NOT_FOUND
        } else if message.contains("rate_limited") {
            StatusCode::TOO_MANY_REQUESTS
        } else if message.contains("conflict")
            || message.contains("submission_pending")
            || message.contains("duplicate_evidence")
        {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        Self(status, message)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"error":{"message":self.1,"status":self.0.as_u16()}})),
        )
            .into_response()
    }
}
