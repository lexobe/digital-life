//! Mock-upstream throughput baseline; excludes LLM latency and sandbox density.
use anyhow::Result;
use axum::{Json, Router, routing::post};
use serde_json::json;
use std::time::Instant;
use survive_core::{ExperimentInput, Store};
use survive_gateway::{Gateway, GatewayConfig, Protocol};

#[tokio::main]
async fn main() -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let upstream = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/chat/completions",post(||async{Json(json!({"id":"bench","object":"chat.completion","model":"mock","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}))}))).await.unwrap()
    });
    let temporary = tempfile::tempdir()?;
    let store = Store::open(temporary.path().join("db"))?;
    let input: ExperimentInput = serde_json::from_value(
        json!({"name":"benchmark","goal":"mock only","initial_prompt":"benchmark","count":8,"initial_tokens":1000000,"reward_budget":0,"rules":[]}),
    )?;
    let (_, agents) = store.create_experiment("fixture", input)?;
    let config: GatewayConfig = serde_json::from_value(
        json!({"providers":[{"name":"mock","base_url":format!("http://{address}"),"models":{"default":"mock"}}]}),
    )?;
    let gateway = Gateway::new(config, store.clone())?;
    let start = Instant::now();
    let mut workers = Vec::new();
    for (agent, _) in agents.clone() {
        for _ in 0..2 {
            let gateway = gateway.clone();
            let id = agent.id.clone();
            workers.push(tokio::spawn(async move{
        let mut samples=Vec::new();for _ in 0..25{let start=Instant::now();let response=gateway.completion(&id,Protocol::Chat,json!({"model":"default","max_tokens":32,"messages":[{"role":"user","content":"hello"}]})).await.map_err(|e|anyhow::anyhow!("{e:?}"))?;
            anyhow::ensure!(response.status().is_success(),"unexpected gateway status");axum::body::to_bytes(response.into_body(),65536).await?;samples.push(start.elapsed().as_micros() as u64);
        }Ok::<_,anyhow::Error>(samples)
    }));
        }
    }
    let mut samples = Vec::new();
    for worker in workers {
        samples.extend(worker.await??)
    }
    let elapsed = start.elapsed();
    samples.sort_unstable();
    let used: u64 = store.list_agents(None)?.iter().map(|a| a.used).sum();
    anyhow::ensure!(used == samples.len() as u64 * 12, "accounting mismatch");
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"fixture":"loopback model, real durable redb accounting","requests":samples.len(),"concurrency":16,"elapsed_ms":elapsed.as_millis(),"requests_per_second":samples.len() as f64/elapsed.as_secs_f64(),"p50_ms":samples[samples.len()/2] as f64/1000.0,"p95_ms":samples[samples.len()*95/100] as f64/1000.0,"settled_tokens":used,"scope":"gateway + local HTTP upstream + durable ledger; not real model latency or sandbox capacity"})
        )?
    );
    upstream.abort();
    Ok(())
}
