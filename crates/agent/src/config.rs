use anyhow::{Context, Result, ensure};
use std::{path::PathBuf, time::Duration};

/// Limits bound model context and working memory, never truncate the disk history.
/// The organism DNA and environment description must fit in the request budget; it is never silently shortened.
#[derive(Clone)]
pub struct Config {
    pub env_url: String,
    pub token: String,
    pub work_dir: PathBuf,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub tool_context_bytes: usize,
    pub max_output_tokens: u32,
    pub max_tool_calls: usize,
    pub http_timeout: Duration,
    pub shell_timeout: Duration,
    pub max_sleep: Duration,
    pub idle_delay: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

impl Config {
    pub fn new(env_url: impl Into<String>, token: impl Into<String>, work_dir: PathBuf) -> Self {
        Self {
            env_url: env_url.into(),
            token: token.into(),
            work_dir,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 8 * 1024 * 1024,
            tool_context_bytes: 4096,
            max_output_tokens: 4096,
            max_tool_calls: 32,
            http_timeout: Duration::from_secs(180),
            shell_timeout: Duration::from_secs(300),
            max_sleep: Duration::from_secs(86400),
            idle_delay: Duration::from_millis(250),
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        }
    }

    pub fn from_env() -> Result<Self> {
        let mut config = Self::new(
            std::env::var("SURVIVE_ENV_URL").unwrap_or_else(|_| "http://10.203.0.1:8089".into()),
            std::env::var("SURVIVE_TOKEN").context("SURVIVE_TOKEN must be set")?,
            std::env::var_os("SURVIVE_WORK_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/work".into()),
        );
        if let Ok(value) = std::env::var("SURVIVE_CONTEXT_BYTES") {
            config.max_request_bytes = value.parse().context("invalid SURVIVE_CONTEXT_BYTES")?;
        }
        if let Ok(value) = std::env::var("SURVIVE_OUTPUT_TOKENS") {
            config.max_output_tokens = value.parse().context("invalid SURVIVE_OUTPUT_TOKENS")?;
        }
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.env_url).context("invalid environment URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "environment URL must be HTTP(S)"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "environment URL must not contain credentials, query, or fragment"
        );
        ensure!(!self.token.is_empty(), "environment token is empty");
        ensure!(
            self.max_request_bytes >= 8192
                && self.max_response_bytes > 0
                && self.tool_context_bytes >= 256,
            "invalid context limits"
        );
        ensure!(
            self.max_output_tokens > 0 && self.max_tool_calls > 0,
            "output/tool limits must be positive"
        );
        ensure!(
            !self.http_timeout.is_zero()
                && !self.shell_timeout.is_zero()
                && !self.max_sleep.is_zero(),
            "timeouts must be positive"
        );
        ensure!(
            !self.backoff_initial.is_zero() && self.backoff_max >= self.backoff_initial,
            "invalid backoff limits"
        );
        Ok(())
    }
}
