//! Read-only operating-system desktop observer. All network traffic stays on the
//! organism's existing authenticated environment API. A frame belongs to one poll ticket.
use crate::{Config, computer, http::private_client};
use anyhow::{Context, Result, ensure};
use reqwest::{Client, StatusCode, header::HeaderValue};
use serde::Deserialize;
use std::time::Duration;
use tokio::time::{sleep, timeout};

const POLL_TIMEOUT: Duration = Duration::from_secs(35);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_DEMAND_BYTES: usize = 2048;

#[derive(Deserialize)]
struct Demand {
    ticket: String,
}

struct Observer {
    client: Client,
    demand_url: String,
    frame_url: String,
    credential: String,
}

impl Observer {
    async fn poll(&self) -> Result<()> {
        let mut reply = self
            .client
            .get(&self.demand_url)
            .bearer_auth(&self.credential)
            .send()
            .await?;
        if reply.status() == StatusCode::NO_CONTENT {
            return Ok(());
        }
        ensure!(
            reply.status() == StatusCode::OK,
            "demand HTTP {}",
            reply.status()
        );
        let mut body = Vec::new();
        while let Some(chunk) = reply.chunk().await? {
            ensure!(
                chunk.len() <= MAX_DEMAND_BYTES - body.len(),
                "oversized observer demand"
            );
            body.extend_from_slice(&chunk);
        }
        let demand: Demand = serde_json::from_slice(&body).context("invalid observer demand")?;
        ensure!(
            !demand.ticket.trim().is_empty() && demand.ticket.len() <= 512,
            "invalid observer ticket"
        );
        let ticket = HeaderValue::from_str(&demand.ticket).context("invalid observer ticket")?;
        let mut temp = tempfile::Builder::new();
        temp.prefix("survive-desktop-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = temp.tempdir_in("/tmp")?;
        let frame = timeout(Duration::from_secs(10), computer::capture(directory.path()))
            .await
            .context("desktop capture timed out")??;
        drop(directory);
        let reply = self
            .client
            .post(&self.frame_url)
            .bearer_auth(&self.credential)
            .header("content-type", "image/jpeg")
            .header("x-survive-desktop-ticket", ticket)
            .timeout(UPLOAD_TIMEOUT)
            .body(frame.bytes)
            .send()
            .await?;
        ensure!(reply.status().is_success(), "frame HTTP {}", reply.status());
        Ok(())
    }
}

/// Poll independently of the mutable organism entrypoint, model and journal.
pub async fn run_desktop_observer(config: Config) -> Result<()> {
    config.validate()?;
    let base = config.env_url.trim_end_matches('/');
    let worker = Observer {
        client: private_client(POLL_TIMEOUT)?,
        demand_url: format!("{base}/env/v2/desktop/demand"),
        frame_url: format!("{base}/env/v2/desktop/frame"),
        credential: config.token,
    };
    let mut delay = Duration::from_secs(1);
    loop {
        match worker.poll().await {
            Ok(()) => {
                delay = Duration::from_secs(1);
                sleep(Duration::from_millis(250)).await;
            }
            Err(error) => {
                // A failed or lost frame is disposable. Only a new demand can
                // authorize another capture; an upload is never retried here.
                if delay == Duration::from_secs(1) {
                    eprintln!("survive desktop observer: {error:#}");
                }
                sleep(delay).await;
                delay = delay.saturating_mul(2).min(Duration::from_secs(8));
            }
        }
    }
}
