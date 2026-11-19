use anyhow::Result;
use survive_agent::{Agent, Config, run_desktop_observer};

fn main() -> Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--desktop-observer")) {
        return observer();
    }
    seed()
}

#[tokio::main(flavor = "current_thread")]
async fn observer() -> Result<()> {
    tokio::select! {
        result = run_desktop_observer(Config::from_env()?) => result,
        result = shutdown() => result,
    }
}

#[tokio::main]
async fn seed() -> Result<()> {
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
