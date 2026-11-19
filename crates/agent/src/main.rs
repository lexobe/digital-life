use anyhow::Result;
use survive_agent::{Agent, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let mut agent = Agent::open(Config::from_env()?)?;
    tokio::select! {
        result = agent.run() => { eprintln!("survive-agent: {:?}", result?); }
        result = shutdown() => { result?; eprintln!("survive-agent: stopped; durable history retained"); }
    }
    Ok(())
}

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
