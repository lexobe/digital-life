use anyhow::{Context, Result, bail};
use survive_sandbox::{SandboxConfig, SandboxManager, SandboxSpec};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        bail!(
            "usage: survive-sandbox CONFIG.toml doctor|setup|restore|prepare SPEC.json|start ID|stop ID|status ID"
        );
    }
    let config: SandboxConfig = toml::from_str(&std::fs::read_to_string(&args[1])?)?;
    let manager = SandboxManager::new(config)?;
    let operand = || args.get(3).context("missing spec path or agent ID");
    match args[2].as_str() {
        "doctor" | "preflight" => {
            manager.doctor().await?;
            println!("sandbox preflight passed");
        }
        "setup" => {
            manager.setup().await?;
            println!("sandbox setup complete");
        }
        "restore" => {
            manager.restore().await?;
            println!("sandbox isolation restored; agents remain stopped");
        }
        "prepare" => {
            let spec: SandboxSpec = serde_json::from_slice(&std::fs::read(operand()?)?)?;
            manager.prepare(&spec).await?;
        }
        "start" => manager.start(operand()?).await?,
        "stop" => manager.stop(operand()?).await?,
        "status" => println!(
            "{}",
            serde_json::to_string(&manager.status(operand()?).await?)?
        ),
        _ => bail!("unknown command"),
    }
    Ok(())
}
