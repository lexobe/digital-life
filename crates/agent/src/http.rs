use crate::{Config, journal::save_json};
use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub(crate) struct Environment {
    client: Client,
    base: String,
    token: String,
    max_response_bytes: usize,
}

pub(crate) struct Reply {
    pub status: u16,
    pub body: Value,
    pub artifact: PathBuf,
}

impl Environment {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(config.http_timeout)
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .build()?,
            base: config.env_url.trim_end_matches('/').to_owned(),
            token: config.token.clone(),
            max_response_bytes: config.max_response_bytes,
        })
    }

    /// Exactly one HTTP attempt. In particular, no hidden retry of a POST.
    /// Bodies are spooled to disk before parsing, including unsuccessful and malformed responses.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        directory: &Path,
    ) -> Result<Reply> {
        tokio::fs::create_dir_all(directory).await?;
        let url = format!("{}{path}", self.base);
        save_json(
            &directory.join("request.json"),
            &json!({"method":method.as_str(), "url":url, "body":body}),
        )?;
        let mut request = self.client.request(method, &url).bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                save_json(
                    &directory.join("transport-error.json"),
                    &json!({"error":error.to_string(), "outcome":"unknown"}),
                )?;
                return Err(error)
                    .context("HTTP transport failure; remote processing may have happened");
            }
        };
        let status = response.status().as_u16();
        let headers: Vec<_> = response
            .headers()
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_bytes().to_vec()))
            .collect();
        save_json(
            &directory.join("response-meta.json"),
            &json!({"status":status, "headers":headers}),
        )?;
        let artifact = directory.join("response.body");
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&artifact)
            .await?;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => file.write_all(&chunk).await?,
                Ok(None) => break,
                Err(error) => {
                    file.sync_all().await?;
                    save_json(
                        &directory.join("transport-error.json"),
                        &json!({"error":error.to_string(), "outcome":"unknown", "partial_body":artifact}),
                    )?;
                    return Err(error)
                        .context("incomplete HTTP body; saved bytes must not be executed");
                }
            }
        }
        file.sync_all().await?;
        let length = file.metadata().await?.len();
        save_json(&directory.join("received.json"), &json!({"bytes":length}))?;
        ensure!(
            length <= self.max_response_bytes as u64,
            "response too large to parse; complete body preserved at {}",
            artifact.display()
        );
        let bytes = tokio::fs::read(&artifact).await?;
        let body = if bytes.is_empty() && (200..300).contains(&status) {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "HTTP {status} returned invalid JSON; body retained at {}",
                    artifact.display()
                )
            })?
        };
        Ok(Reply {
            status,
            body,
            artifact,
        })
    }
}
