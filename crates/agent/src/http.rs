use crate::{Config, journal::save_json};
use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
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

pub(crate) fn private_client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .build()?)
}

impl Environment {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: private_client(config.http_timeout)?,
            base: config.env_url.trim_end_matches('/').to_owned(),
            token: config.token.clone(),
            max_response_bytes: config.max_response_bytes,
        })
    }

    /// A read-only wake hint. The next turn reads the authoritative message from /self.
    /// Polling during a long sleep does not create per-poll history artifacts.
    pub async fn has_pending_message(&self) -> Result<bool> {
        let mut response = self
            .client
            .get(format!(
                "{}/env/v2/messages?pending=true&limit=1",
                self.base
            ))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= self.max_response_bytes.min(16 * 1024),
                "oversized pending message response"
            );
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes)?;
        Ok(!body["messages"]
            .as_array()
            .context("missing pending messages")?
            .is_empty())
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
        self.request_recorded(method, path, body, body, directory)
            .await
    }

    pub async fn request_recorded(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        recorded: Option<&Value>,
        directory: &Path,
    ) -> Result<Reply> {
        tokio::fs::create_dir_all(directory).await?;
        let url = format!("{}{path}", self.base);
        save_json(
            &directory.join("request.json"),
            &json!({"method":method.as_str(), "url":url, "body":recorded}),
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
