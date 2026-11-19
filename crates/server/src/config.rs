use serde::{Deserialize, Deserializer};
use std::{net::SocketAddr, path::PathBuf};
use survive_gateway::GatewayConfig;
use survive_sandbox::SandboxConfig;

#[derive(Clone, Deserialize)]
pub struct Config {
    pub data_dir: PathBuf,
    #[serde(default = "listen")]
    pub listen: SocketAddr,
    /// Canonical creator origin for absolute links and examples in the public guide.
    #[serde(default, deserialize_with = "read_public_origin")]
    pub public_origin: Option<String>,
    #[serde(default = "env_listen")]
    pub env_listen: SocketAddr,
    pub creators: Vec<Creator>,
    pub gateway: GatewayConfig,
    pub sandbox: Option<SandboxConfig>,
    pub source_dir: Option<PathBuf>,
    /// Built Rust/WASM observer console. No credentials are included in assets.
    #[serde(default)]
    pub console_dir: Option<PathBuf>,
    #[serde(default)]
    pub test_mode: bool,
}
#[derive(Clone, Deserialize)]
pub struct Creator {
    pub id: String,
    pub token_env: String,
}
fn listen() -> SocketAddr {
    "127.0.0.1:8088".parse().unwrap()
}
fn env_listen() -> SocketAddr {
    "10.203.0.1:8089".parse().unwrap()
}

fn read_public_origin<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let invalid = || {
        serde::de::Error::custom(
            "public_origin must be an HTTP(S) origin containing only a host and optional port",
        )
    };
    let url = url::Url::parse(&raw).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.host_str().is_some_and(|host| {
            host.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b))
        })
    {
        return Err(invalid());
    }
    Ok(Some(url.origin().ascii_serialization()))
}
