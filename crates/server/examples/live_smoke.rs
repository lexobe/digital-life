//! Run with a private acceptance database, the loopback fixture, and real youki.
use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
struct Fixture {
    client: Client,
    base: String,
    key: String,
}
impl Fixture {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.key);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        let value: Value = response.json().await?;
        ensure!(status.is_success(), "{path} failed: {status} {value}");
        Ok(value)
    }
    async fn until_stopped(&self, path: &str, reason: &str) -> Result<Value> {
        let start = Instant::now();
        loop {
            let result = self.call(Method::GET, path, None).await?;
            let a = &result["organism"];
            ensure!(
                a["state"] != "failed",
                "sandbox failed: {}",
                a["last_error"]
            );
            if a["state"] == "depleted"
                && a["runtime_running"] == false
                && a["life"]["stop_reasons"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|r| *r == reason))
            {
                return Ok(a.clone());
            }
            ensure!(
                start.elapsed() < Duration::from_secs(240),
                "timed out waiting for {reason}: {a}"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    ensure!(
        std::env::var("SURVIVE_LIVE_SMOKE").as_deref() == Ok("YES"),
        "SURVIVE_LIVE_SMOKE=YES required"
    );
    let base = std::env::var("SURVIVE_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8088".into());
    ensure!(
        reqwest::Url::parse(&base)?.host_str() == Some("127.0.0.1"),
        "local acceptance fixture required"
    );
    let f = Fixture {
        base,
        key: std::env::var("SURVIVE_CREATOR_TOKEN")?,
        client: Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()?,
    };
    let env = f
        .call(
            Method::POST,
            "/api/v2/environments",
            Some(json!({"description":"Report your progress to the creator."})),
        )
        .await?;
    let env_id = env["environment"]["id"]
        .as_str()
        .context("no environment id")?;
    let batch=f.call(Method::POST,&format!("/api/v2/environments/{env_id}/organisms"),Some(json!({"count":1,"defaults":{"initial_energy":50000,"lifetime_seconds":20,"default_model":"default","dna":"Record an observation."}}))).await?;
    let id = batch["organisms"][0]["id"]
        .as_str()
        .context("no organism id")?;
    let path = format!("/api/v2/organisms/{id}");
    let first = f.until_stopped(&path, "energy").await?;
    ensure!(
        first["used"] == 50000
            && first["life"]["remaining_lifetime_ms"].as_u64().unwrap_or(0) > 1000,
        "unexpected first stop: {first}"
    );
    let reports = f
        .call(Method::GET, &format!("{path}/reports"), None)
        .await?;
    let rid = reports["reports"][0]["id"].as_str().context("no report")?;
    let full = f
        .call(Method::GET, &format!("{path}/reports/{rid}"), None)
        .await?;
    ensure!(
        full["report"]["snapshots"][0]["data"] == "ZnJvemVuIHNtb2tlIGFydGlmYWN0",
        "immutable snapshot mismatch"
    );
    let grants = format!("{path}/grants");
    let reward = json!({"energy":50000,"idempotency_key":"recharge","report_id":rid});
    let a = f.call(Method::POST, &grants, Some(reward.clone())).await?;
    let b = f.call(Method::POST, &grants, Some(reward)).await?;
    ensure!(a["grant"]["id"] == b["grant"]["id"], "duplicate grant");
    let expired = f.until_stopped(&path, "lifetime").await?;
    ensure!(
        expired["used"] == 50300 && expired["granted"] == 100000,
        "unexpected time stop {expired}"
    );
    let file = f
        .call(Method::GET, &format!("{path}/files?path=resumed.txt"), None)
        .await?;
    ensure!(file["data"].is_string(), "missing retained workspace");
    f.call(
        Method::POST,
        &grants,
        Some(json!({"lifetime_seconds":60,"idempotency_key":"extend"})),
    )
    .await?;
    let last = f.until_stopped(&path, "energy").await?;
    ensure!(
        last["used"] == 100000 && last["generation"] == 3,
        "unexpected final state {last}"
    );
    println!(
        "{}",
        json!({"fixture":true,"organism_id":id,"environment_id":env_id,"report_id":rid,"generation":3,"used":100000,"result":"real youki reported, stopped on energy, resumed from idempotent grant, stopped on runtime expiry, revived on extension, stopped on energy"})
    );
    Ok(())
}
