//! A continuous, restartable agent. All durable state lives under WORK/.agent/history.
//! Tools run only after a complete non-streamed response and a durable intent record.
mod config;
mod http;
mod journal;
mod responses;
mod tools;

use anyhow::{Context, Result, ensure};
pub use config::Config;
use http::Environment;
use journal::{Action, ActionStatus, Journal, Stage, Turn, atomic_write, save_json};
use reqwest::Method;
use serde_json::{Value, json};
use std::{collections::HashSet, fs};

const INSTRUCTIONS: &str = "You are an autonomous digital organism. Read your environment description and decide how to live within it. Inference costs energy according to the model price list. Your lifetime is consumed while the sandbox runs. You can send reports to your creator, who may grant energy or lifetime. You can name yourself, choose a model, and edit your DNA (dna.md), source (source/), tools and boot manifest (boot.json). Every inference reloads your DNA. Use the environment tools to inspect your life, model prices and peers. Hourly heartbeats record your condition. Choose actions at your own pace. Full action and inference history is under .agent/history/. Tools run sequentially. If an earlier action has uncertain effects, inspect its artifacts before attempting it again.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    Advanced { turn: u64 },
    Backoff { reason: String },
    WaitingForBalance,
    Depleted,
    RestartRequested,
}

/// Holds an exclusive work-directory lock. Dropping a step during a side effect leaves
/// the action running in the journal; a newly opened Agent will report it as uncertain.
pub struct Agent {
    config: Config,
    environment: Environment,
    journal: Journal,
}

impl Agent {
    pub fn open(mut config: Config) -> Result<Self> {
        config.validate()?;
        config.work_dir = fs::canonicalize(&config.work_dir)
            .context("work directory must be prepared before startup")?;
        let environment = Environment::new(&config)?;
        let journal = Journal::open(&config.work_dir)?;
        Ok(Self {
            config,
            environment,
            journal,
        })
    }

    /// One inference plus its complete tool batch, or recovery of a pending batch.
    /// Network failures are recorded and returned as Backoff. Local persistence and
    /// configuration failures are fatal: proceeding could duplicate side effects.
    pub async fn step(&mut self) -> Result<StepOutcome> {
        if let Some(&id) = self.journal.pending.front() {
            let mut turn = self.journal.load(id)?;
            match turn.stage {
                Stage::Preparing => {
                    turn.note = Some("Interrupted before inference; no model request started. Prior artifacts retained.".into());
                    self.journal.complete(&mut turn)?;
                    return Ok(StepOutcome::Advanced { turn: id });
                }
                Stage::RequestStarted => {
                    let directory = self.journal.turn_dir(id).join("model");
                    if directory.join("received.json").exists() {
                        let metadata: Value = serde_json::from_slice(&fs::read(
                            directory.join("response-meta.json"),
                        )?)?;
                        let length = fs::metadata(directory.join("response.body"))?.len();
                        if metadata["status"]
                            .as_u64()
                            .is_some_and(|s| (200..300).contains(&s))
                            && length <= self.config.max_response_bytes as u64
                        {
                            let parsed =
                                serde_json::from_slice(&fs::read(directory.join("response.body"))?)
                                    .context("invalid stored model JSON")
                                    .and_then(|body| {
                                        prepare_actions(
                                            &mut turn,
                                            &body,
                                            self.config.max_tool_calls,
                                        )
                                    });
                            if parsed.is_ok() {
                                self.journal.save(&turn)?;
                                return self.finish_tools(turn).await;
                            }
                        }
                    }
                    turn.note = Some("Model request outcome uncertain or incomplete. It was not replayed; no partial tool calls were executed. Inspect model/ artifacts and environment ledger.".into());
                    self.journal.complete(&mut turn)?;
                    return Ok(StepOutcome::Advanced { turn: id });
                }
                Stage::Ready => {
                    for (index, action) in turn.actions.iter_mut().enumerate() {
                        if action.status == ActionStatus::Running {
                            let result_path =
                                self.journal.action_dir(id, index).join("result.json");
                            if result_path.exists() {
                                let result: Value =
                                    serde_json::from_slice(&fs::read(result_path)?)?;
                                action.status = if result["outcome"] == "uncertain" {
                                    ActionStatus::Uncertain
                                } else {
                                    ActionStatus::Done
                                };
                                action.result = Some(result);
                            } else {
                                action.result = Some(
                                    json!({"ok":false,"outcome":"uncertain","error":"Interrupted after durable intent. This action may already have had effects and was NOT replayed.","artifacts":self.journal.action_dir(id,index)}),
                                );
                                save_json(&result_path, &action.result)?;
                                action.status = ActionStatus::Uncertain;
                            }
                        }
                    }
                    self.journal.save(&turn)?;
                    return self.finish_tools(turn).await;
                }
                Stage::Complete => anyhow::bail!("completed turn in pending queue"),
            }
        }

        let mut turn = self.journal.new_turn()?;
        let directory = self.journal.turn_dir(turn.id);
        let reply = match self
            .environment
            .request(Method::GET, "/env/v2/self", None, &directory.join("self"))
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                return self.fail_turn(&mut turn, format!("self-info request failed: {error:#}"));
            }
        };
        if !(200..300).contains(&reply.status) {
            return self.fail_turn(
                &mut turn,
                format!(
                    "self-info HTTP {}; full response: {}",
                    reply.status,
                    reply.artifact.display()
                ),
            );
        }
        let info = reply.body;
        let life = &info["life"];
        let (Some(balance), Some(available), Some(remaining), Some(default_model)) = (
            life["energy_balance"].as_u64(),
            life["energy_available"].as_u64(),
            life["remaining_lifetime_ms"].as_u64(),
            info["organism"]["default_model"].as_str(),
        ) else {
            return self.fail_turn(
                &mut turn,
                "Incomplete life or model data in self response.".into(),
            );
        };
        if !info["environment"]["description"].is_string() {
            return self.fail_turn(
                &mut turn,
                "Missing immutable environment description.".into(),
            );
        }
        if balance == 0 || remaining == 0 {
            turn.note = Some("The organism has exhausted its energy or running time.".into());
            self.journal.complete(&mut turn)?;
            return Ok(StepOutcome::Depleted);
        }
        if available == 0 {
            turn.note = Some("Available energy is reserved by a model call.".into());
            self.journal.complete(&mut turn)?;
            return Ok(StepOutcome::WaitingForBalance);
        }
        let prompt = fs::read_to_string(self.config.work_dir.join("dna.md"))
            .context("reading organism DNA")?;
        atomic_write(&directory.join("dna.md"), prompt.as_bytes())?;
        let preference = match fs::read_to_string(self.config.work_dir.join("model.txt")) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("reading model selection"),
        };
        let model = preference
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default_model);
        let Some(request) = self.build_request(&prompt, &info, model, turn.id)? else {
            turn.note = Some(format!(
                "Waiting for sufficient energy for {model} at its current quoted rates."
            ));
            self.journal.complete(&mut turn)?;
            return Ok(StepOutcome::WaitingForBalance);
        };
        let path = if request.get("input").is_some() {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        save_json(&directory.join("model-request.json"), &request)?;
        turn.stage = Stage::RequestStarted;
        self.journal.save(&turn)?;
        let reply = match self
            .environment
            .request(Method::POST, path, Some(&request), &directory.join("model"))
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                return self.fail_turn(
                    &mut turn,
                    format!(
                        "Model request outcome may be uncertain; no automatic replay: {error:#}"
                    ),
                );
            }
        };
        if !(200..300).contains(&reply.status) {
            return self.fail_turn(
                &mut turn,
                format!(
                    "Model HTTP {}; no automatic replay; inspect {} and ledger",
                    reply.status,
                    reply.artifact.display()
                ),
            );
        }
        if let Err(error) = prepare_actions(&mut turn, &reply.body, self.config.max_tool_calls) {
            return self.fail_turn(
                &mut turn,
                format!("Model response rejected; no tools executed: {error:#}"),
            );
        }
        self.journal.save(&turn)?;
        self.finish_tools(turn).await
    }

    fn fail_turn(&mut self, turn: &mut Turn, reason: String) -> Result<StepOutcome> {
        turn.note = Some(reason.clone());
        self.journal.complete(turn)?;
        Ok(StepOutcome::Backoff { reason })
    }

    async fn finish_tools(&mut self, mut turn: Turn) -> Result<StepOutcome> {
        for index in 0..turn.actions.len() {
            if turn.actions[index].status != ActionStatus::Planned {
                continue;
            }
            let directory = self.journal.action_dir(turn.id, index);
            save_json(&directory.join("intent.json"), &turn.actions[index])?;
            turn.actions[index].status = ActionStatus::Running;
            // Must be durable before executing even a file write or starting a shell.
            self.journal.save(&turn)?;
            let action = &turn.actions[index];
            let result = tools::execute(
                &self.config,
                &self.environment,
                &action.name,
                &action.arguments,
                &directory,
            )
            .await;
            save_json(&directory.join("result.json"), &result.value)?;
            turn.actions[index].result = Some(result.value);
            turn.actions[index].status = ActionStatus::Done;
            self.journal.save(&turn)?;
            if result.restart {
                if index + 1 == turn.actions.len() {
                    self.journal.complete(&mut turn)?;
                }
                return Ok(StepOutcome::RestartRequested);
            }
        }
        self.journal.complete(&mut turn)?;
        Ok(StepOutcome::Advanced { turn: turn.id })
    }

    fn build_request(
        &self,
        prompt: &str,
        info: &Value,
        model: &str,
        turn_id: u64,
    ) -> Result<Option<Value>> {
        let directory = self.journal.turn_dir(turn_id);
        let mut messages = vec![
            json!({"role":"system","content":INSTRUCTIONS}),
            json!({"role":"system","content":prompt}),
            json!({"role":"user","content":format!("Environment description:\n{}", info["environment"]["description"].as_str().unwrap_or_default())}),
            json!({"role":"user","content":format!("Work directory: {}\nCurrent environment: {}",self.config.work_dir.display(), bounded_value(info, self.config.tool_context_bytes, &directory.join("self/response.body")))}),
        ];
        let tail = json!({"role":"user","content":format!("Continue autonomously. Current iteration: {turn_id}. Full history directory: {}. Inspect earlier records when needed.",self.journal.root.display())});
        let mut request = json!({"model":model,"stream":false,"max_tokens":self.config.max_output_tokens,"tools":tools::definitions(),"tool_choice":"auto","messages":[]});
        let use_responses = model.starts_with("gpt-6");
        let message_key = if use_responses {
            messages = responses::messages(&messages)?;
            request = responses::request(&request)?;
            "input"
        } else {
            "messages"
        };
        request[message_key] = json!([messages.clone(), vec![tail.clone()]].concat());
        let base_length = serde_json::to_vec(&request)?.len();
        ensure!(
            base_length <= self.config.max_request_bytes,
            "DNA and environment exceed max_request_bytes; full texts retained"
        );
        let mut context_limit = self.config.max_request_bytes;
        if let Some(quote) = info["models"]
            .as_array()
            .and_then(|models| models.iter().find(|entry| entry["id"] == model))
        {
            let input_rate = quote["input"]
                .as_u64()
                .filter(|&v| v > 0)
                .context("invalid input energy rate")?;
            let output_rate = quote["output"]
                .as_u64()
                .filter(|&v| v > 0)
                .context("invalid output energy rate")?;
            let energy = info["life"]["energy_available"]
                .as_u64()
                .context("missing energy balance")?;
            let input_floor = info["minimum_input_reservation"].as_u64().unwrap_or(1024);
            // The gateway reserves one input token per serialized byte for this
            // stateless call and charges actual usage after the provider replies.
            let base_hold = input_rate.saturating_mul(input_floor.max(base_length as u64));
            let output_cap = (energy.saturating_sub(base_hold) / output_rate)
                .min(u64::from(self.config.max_output_tokens));
            if output_cap < if use_responses { 16 } else { 1 } {
                return Ok(None);
            }
            let key = if use_responses {
                "max_output_tokens"
            } else {
                "max_tokens"
            };
            request[key] = json!(output_cap);
            let affordable_bytes = (energy - output_cap * output_rate) / input_rate;
            context_limit =
                context_limit.min(usize::try_from(affordable_bytes).unwrap_or(usize::MAX));
        }
        let mut remaining = context_limit - base_length;
        let mut bundles = Vec::new();
        // Read a bounded number of recent turn states, never entire history bodies.
        for &id in self
            .journal
            .ids
            .iter()
            .rev()
            .filter(|&&id| id < turn_id)
            .take(32)
        {
            if !self.journal.turn_dir(id).join("state.json").exists() {
                continue;
            }
            let previous = self.journal.load(id)?;
            let mut bundle = Vec::new();
            if let Some(assistant) = previous.assistant {
                bundle.push(assistant);
                for (index, action) in previous.actions.iter().enumerate() {
                    let result = action
                        .result
                        .as_ref()
                        .context("completed assistant turn has a missing tool result")?;
                    bundle.push(json!({"role":"tool","tool_call_id":action.id,"content":bounded_value(result,self.config.tool_context_bytes,&self.journal.action_dir(id,index).join("result.json"))}));
                }
            }
            if let Some(note) = previous.note {
                bundle.push(json!({"role":"user","content":format!("Recorded iteration {id}: {note}. History: {}", self.journal.turn_dir(id).display())}));
            }
            if use_responses {
                bundle = responses::messages(&bundle)?;
            }
            let cost = serde_json::to_vec(&bundle)?.len() + 1;
            if cost <= remaining {
                remaining -= cost;
                bundles.push(bundle);
            }
        }
        for bundle in bundles.into_iter().rev() {
            messages.extend(bundle);
        }
        messages.push(tail);
        request[message_key] = Value::Array(messages);
        ensure!(
            serde_json::to_vec(&request)?.len() <= context_limit,
            "request context accounting overflow"
        );
        Ok(Some(request))
    }

    /// Transient gateway errors wait with capped exponential backoff, then start a new
    /// recorded iteration. No network request or uncertain action is invisibly replayed.
    pub async fn run(&mut self) -> Result<StepOutcome> {
        let mut delay = self.config.backoff_initial;
        loop {
            match self.step().await? {
                outcome @ StepOutcome::RestartRequested => {
                    return Ok(outcome);
                }
                StepOutcome::Backoff { reason } => {
                    eprintln!(
                        "survive-agent: {reason}; next iteration in {}ms",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(self.config.backoff_max);
                }
                // The host owns cgroup termination. A creator grant can race our last
                // self read; keep the core able to observe it until actually stopped.
                StepOutcome::WaitingForBalance | StepOutcome::Depleted => {
                    tokio::time::sleep(self.config.backoff_max).await;
                }
                StepOutcome::Advanced { .. } => {
                    delay = self.config.backoff_initial;
                    tokio::time::sleep(self.config.idle_delay).await;
                }
            }
        }
    }
}

fn prepare_actions(turn: &mut Turn, response: &Value, max_calls: usize) -> Result<()> {
    let converted;
    let response = if response["object"] == "response" {
        converted = responses::completion(response)?;
        &converted
    } else {
        response
    };
    let choices = response["choices"].as_array().context("missing choices")?;
    ensure!(choices.len() == 1, "expected one completion choice");
    let choice = &choices[0];
    let message = &choice["message"];
    ensure!(message["role"] == "assistant", "expected assistant message");
    ensure!(
        message.get("function_call").is_none_or(Value::is_null),
        "legacy function_call is unsupported"
    );
    ensure!(
        message
            .get("content")
            .is_none_or(|v| v.is_null() || v.is_string()),
        "assistant content must be text or null"
    );
    let calls = match message.get("tool_calls") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(calls)) => calls.as_slice(),
        _ => anyhow::bail!("tool_calls must be an array"),
    };
    ensure!(calls.len() <= max_calls, "too many tool calls");
    if calls.is_empty() {
        ensure!(
            choice["finish_reason"] == "stop",
            "incomplete or unsupported completion finish_reason"
        );
    } else {
        ensure!(
            choice["finish_reason"] == "tool_calls",
            "tool arguments might be incomplete (finish_reason is not tool_calls)"
        );
    }
    let mut seen = HashSet::new();
    let mut actions = Vec::new();
    for call in calls {
        ensure!(call["type"] == "function", "unsupported tool call type");
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("missing tool call ID")?;
        ensure!(seen.insert(id), "duplicate tool call ID");
        let function = &call["function"];
        let name = function["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .context("missing function name")?;
        let arguments = function["arguments"]
            .as_str()
            .context("missing complete argument string")?;
        let parsed: Value =
            serde_json::from_str(arguments).context("incomplete/invalid function arguments")?;
        ensure!(parsed.is_object(), "tool arguments must be a JSON object");
        actions.push(Action {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            status: ActionStatus::Planned,
            result: None,
        });
    }
    // Validate the entire batch before making any call executable. Preserve unknown
    // provider fields in raw response.body, while replay context uses standard fields.
    let mut assistant = json!({"role":"assistant","content":message.get("content").cloned().unwrap_or(Value::Null)});
    if !calls.is_empty() {
        assistant["tool_calls"] = Value::Array(calls.to_vec());
    }
    turn.assistant = Some(assistant);
    turn.actions = actions;
    turn.stage = Stage::Ready;
    Ok(())
}

fn bounded_value(value: &Value, limit: usize, artifact: &std::path::Path) -> String {
    let text = value.to_string();
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"truncated":true,"preview":&text[..end],"full_artifact":artifact}).to_string()
}
