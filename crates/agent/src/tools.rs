use crate::{Config, http::Environment, journal::atomic_write};
use anyhow::{Context, Result, bail, ensure};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use survive_core::ServiceRegistration;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    process::Command,
};

pub(crate) fn definitions() -> Value {
    let object = |properties: Value, required: &[&str]| json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false});
    let string = || json!({"type":"string"});
    let integer = || json!({"type":"integer", "minimum":0});
    let tool = |name: &str, description: &str, parameters: Value| json!({"type":"function", "function":{"name":name,"description":description,"parameters":parameters}});
    json!([
        tool(
            "shell",
            "Run /bin/sh -c in the sandbox. Full stdout/stderr are saved as files. May install tools, compile /work/source, and replace /work/boot.json. Timeout may leave partial side effects; never assume failure means nothing happened.",
            object(
                json!({"command":string(),"cwd":string(),"timeout_secs":integer()}),
                &["command"]
            )
        ),
        tool(
            "read_file",
            "Read a file chunk, including source, boot.json, dna.md, or full history. Relative paths start at the work directory. Use next_offset for more bytes.",
            object(
                json!({"path":string(),"offset":integer(),"max_bytes":{"type":"integer","minimum":1,"maximum":65536}}),
                &["path"]
            )
        ),
        tool(
            "write_file",
            "Atomically replace a UTF-8 file, creating parent directories. Source and launch manifest are writable; changes are not repaired or reverted.",
            object(
                json!({"path":string(),"content":string()}),
                &["path", "content"]
            )
        ),
        tool(
            "env",
            "Read your environment, life, events, energy ledger, priced models, peers or reports. Full responses are saved locally.",
            object(
                json!({"resource":{"type":"string","enum":["self","life","events","ledger","models","peers","reports"]},"after":string(),"limit":{"type":"integer","minimum":1,"maximum":1000}}),
                &["resource"]
            )
        ),
        tool(
            "report",
            "Send a one-way report to your creator with optional snapshots of workspace files. Use a stable idempotency key for each report.",
            object(
                json!({"idempotency_key":string(),"body":string(),"files":{"type":"array","items":string()}}),
                &["idempotency_key", "body"]
            )
        ),
        tool(
            "name",
            "Choose your own display name.",
            object(json!({"name":string()}), &["name"])
        ),
        tool(
            "choose_model",
            "Select the model ID for future inference. Use env models to view your choices and their energy prices.",
            object(json!({"model":string()}), &["model"])
        ),
        tool(
            "service",
            "Register a listening sandbox service. Does not launch a process.",
            object(
                json!({"port":{"type":"integer","minimum":1,"maximum":65535},"name":string()}),
                &["port", "name"]
            )
        ),
        tool(
            "sleep",
            "Sleep for seconds (maximum 86400 by default). Interrupted sleeps are recorded as uncertain and are not repeated automatically.",
            object(json!({"seconds":integer()}), &["seconds"])
        ),
        tool(
            "restart",
            "Ask the environment to reboot using the current boot.json argv. Write/build the new entrypoint first. Pending unstarted actions remain on disk. No automatic repair or fallback.",
            object(json!({}), &[])
        )
    ])
}

pub(crate) struct ToolResult {
    pub value: Value,
    pub restart: bool,
}

pub(crate) async fn execute(
    config: &Config,
    environment: &Environment,
    name: &str,
    arguments: &str,
    directory: &Path,
) -> ToolResult {
    match execute_inner(config, environment, name, arguments, directory).await {
        Ok(result) => result,
        Err(error) => ToolResult {
            value: json!({"ok":false,"error":format!("{error:#}"),"outcome":"unknown_or_not_performed","warning":"Do not assume a failed action had no effects. Inspect artifacts/environment before any new attempt.","artifacts":directory}),
            restart: false,
        },
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    command: String,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: u64,
    max_bytes: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvArgs {
    resource: String,
    after: Option<String>,
    limit: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    idempotency_key: String,
    body: String,
    #[serde(default)]
    files: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelArgs {
    model: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepArgs {
    seconds: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

fn resolve(config: &Config, path: &str) -> PathBuf {
    config.work_dir.join(path)
}

async fn execute_inner(
    config: &Config,
    environment: &Environment,
    name: &str,
    arguments: &str,
    directory: &Path,
) -> Result<ToolResult> {
    tokio::fs::create_dir_all(directory).await?;
    let mut restart = false;
    let value = match name {
        "shell" => {
            let args: ShellArgs = serde_json::from_str(arguments)?;
            let timeout = args
                .timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(config.shell_timeout);
            ensure!(
                !timeout.is_zero() && timeout <= config.shell_timeout,
                "shell timeout must be between 1 and {} seconds",
                config.shell_timeout.as_secs()
            );
            let stdout_path = directory.join("stdout");
            let stderr_path = directory.join("stderr");
            let stdout = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stdout_path)?;
            let stderr = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stderr_path)?;
            let mut command = Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(&args.command)
                .current_dir(
                    args.cwd
                        .as_deref()
                        .map(|p| resolve(config, p))
                        .unwrap_or_else(|| config.work_dir.clone()),
                )
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr)
                .kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);
            let mut child = command.spawn().context("starting shell")?;
            let pid = child.id();
            let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
                Ok(status) => (status?, false),
                Err(_) => {
                    #[cfg(unix)]
                    if let Some(pid) = pid {
                        // Child started its own group; timeout also stops its descendants.
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    let _ = child.kill().await;
                    (child.wait().await?, true)
                }
            };
            std::fs::File::open(&stdout_path)?.sync_all()?;
            std::fs::File::open(&stderr_path)?.sync_all()?;
            json!({"ok":status.success() && !timed_out,"exit_code":status.code(),"timed_out":timed_out,"pid":pid,
                "stdout":preview(&stdout_path, config.tool_context_bytes).await?, "stderr":preview(&stderr_path, config.tool_context_bytes).await?,
                "stdout_path":stdout_path,"stderr_path":stderr_path,"outcome":if timed_out {"partial_effects_possible"} else {"finished"}})
        }
        "read_file" => {
            let args: ReadArgs = serde_json::from_str(arguments)?;
            let limit = args.max_bytes.unwrap_or(16384);
            ensure!((1..=65536).contains(&limit), "max_bytes must be 1..65536");
            let path = resolve(config, &args.path);
            let mut file = tokio::fs::File::open(&path).await?;
            let length = file.metadata().await?.len();
            file.seek(std::io::SeekFrom::Start(args.offset)).await?;
            let mut bytes = Vec::new();
            file.take(limit as u64).read_to_end(&mut bytes).await?;
            let next_offset = args
                .offset
                .checked_add(bytes.len() as u64)
                .context("offset overflow")?;
            // Raw bytes preserve binary/non-UTF8 data; text is a convenience preview.
            json!({"ok":true,"path":path,"offset":args.offset,"next_offset":next_offset,"eof":next_offset >= length,"text":String::from_utf8_lossy(&bytes),"bytes":bytes})
        }
        "write_file" => {
            let args: WriteArgs = serde_json::from_str(arguments)?;
            let path = resolve(config, &args.path);
            atomic_write(&path, args.content.as_bytes())?;
            json!({"ok":true,"path":path,"bytes":args.content.len()})
        }
        "env" => {
            let args: EnvArgs = serde_json::from_str(arguments)?;
            ensure!(
                matches!(
                    args.resource.as_str(),
                    "self" | "life" | "events" | "ledger" | "models" | "peers" | "reports"
                ),
                "unknown environment resource"
            );
            let mut url = reqwest::Url::parse(&format!("http://local/env/v2/{}", args.resource))?;
            if let Some(after) = args.after {
                url.query_pairs_mut().append_pair("after", &after);
            }
            if let Some(limit) = args.limit {
                ensure!((1..=1000).contains(&limit), "limit must be 1..1000");
                url.query_pairs_mut()
                    .append_pair("limit", &limit.to_string());
            }
            let path = format!(
                "{}{}",
                url.path(),
                url.query()
                    .map(|query| format!("?{query}"))
                    .unwrap_or_default()
            );
            request_result(environment, Method::GET, &path, None, directory).await?
        }
        "report" => {
            let args: ReportArgs = serde_json::from_str(arguments)?;
            let body = json!({"body":args.body, "idempotency_key":args.idempotency_key, "files":args.files});
            request_result(
                environment,
                Method::POST,
                "/env/v2/reports",
                Some(&body),
                directory,
            )
            .await?
        }
        "name" => {
            let args: NameArgs = serde_json::from_str(arguments)?;
            request_result(
                environment,
                Method::POST,
                "/env/v2/name",
                Some(&json!({"name":args.name})),
                directory,
            )
            .await?
        }
        "choose_model" => {
            let args: ModelArgs = serde_json::from_str(arguments)?;
            let catalog = environment
                .request(
                    Method::GET,
                    "/env/v2/models",
                    None,
                    &directory.join("catalog"),
                )
                .await?;
            ensure!(
                (200..300).contains(&catalog.status),
                "cannot load model prices"
            );
            let models = catalog.body["models"]
                .as_array()
                .context("invalid model catalog")?;
            ensure!(
                models
                    .iter()
                    .any(|entry| entry["id"].as_str() == Some(&args.model)),
                "model unavailable"
            );
            atomic_write(&config.work_dir.join("model.txt"), args.model.as_bytes())?;
            json!({"ok":true,"next_model":args.model})
        }
        "service" => {
            let input: ServiceRegistration = serde_json::from_str(arguments)?;
            ensure!(
                input.port > 0 && !input.name.trim().is_empty(),
                "service needs a nonzero port and name"
            );
            request_result(
                environment,
                Method::POST,
                "/env/v2/services",
                Some(&serde_json::to_value(input)?),
                directory,
            )
            .await?
        }
        "sleep" => {
            let args: SleepArgs = serde_json::from_str(arguments)?;
            let duration = Duration::from_secs(args.seconds);
            ensure!(
                duration <= config.max_sleep,
                "sleep exceeds configured maximum"
            );
            tokio::time::sleep(duration).await;
            json!({"ok":true,"slept_seconds":args.seconds})
        }
        "restart" => {
            let _: EmptyArgs = serde_json::from_str(arguments)?;
            let result = request_result(
                environment,
                Method::POST,
                "/env/v2/restart",
                Some(&json!({})),
                directory,
            )
            .await?;
            restart = result.get("ok").and_then(Value::as_bool) == Some(true);
            result
        }
        _ => bail!("unknown tool: {name}"),
    };
    Ok(ToolResult { value, restart })
}

async fn request_result(
    environment: &Environment,
    method: Method,
    path: &str,
    body: Option<&Value>,
    directory: &Path,
) -> Result<Value> {
    let reply = environment
        .request(method, path, body, &directory.join("http"))
        .await?;
    Ok(
        json!({"ok":(200..300).contains(&reply.status),"http_status":reply.status,"data":reply.body,"artifact":reply.artifact}),
    )
}

async fn preview(path: &Path, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
