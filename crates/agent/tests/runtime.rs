use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use survive_agent::{Agent, Config, StepOutcome};
use tempfile::TempDir;
use tokio::{sync::Notify, task::JoinHandle};

const TOKEN: &str = "mock-token-not-a-real-credential";

#[derive(Clone)]
struct Recorded {
    method: String,
    path: String,
    body: Value,
    at: Instant,
}

struct MockState {
    requests: Mutex<Vec<Recorded>>,
    completions: Mutex<VecDeque<(u16, String)>>,
    info: Mutex<Value>,
    block_service: bool,
    service_started: Notify,
    deplete_after: usize,
}

struct Mock {
    state: Arc<MockState>,
    task: JoinHandle<()>,
    url: String,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn new(completions: Vec<Value>) -> Self {
        Self::configured(
            completions
                .into_iter()
                .map(|v| (200, v.to_string()))
                .collect(),
            false,
            0,
        )
        .await
    }
    async fn configured(
        completions: Vec<(u16, String)>,
        block_service: bool,
        deplete_after: usize,
    ) -> Self {
        let state = Arc::new(MockState {
            requests: Mutex::new(vec![]),
            completions: Mutex::new(completions.into()),
            info: Mutex::new(
                json!({"organism":{"id":"a1","state":"running","default_model":"mock-logical-model"},"environment":{"id":"e1","description":"Explore and report useful work."},"life":{"energy_balance":10000,"energy_available":10000,"remaining_lifetime_ms":3600000}}),
            ),
            block_service,
            service_started: Notify::new(),
            deplete_after,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().fallback(handle).with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { state, task, url }
    }
    fn requests(&self, path: &str) -> Vec<Recorded> {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }
}

async fn handle(State(state): State<Arc<MockState>>, request: Request) -> Response {
    if request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {TOKEN}"))
    {
        return (StatusCode::UNAUTHORIZED, "missing mock bearer token").into_response();
    }
    let method = request.method().to_string();
    let path = request.uri().to_string();
    let bytes = to_bytes(request.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    state.requests.lock().unwrap().push(Recorded {
        method,
        path: path.clone(),
        body,
        at: Instant::now(),
    });
    if matches!(path.as_str(), "/v1/chat/completions" | "/v1/responses") {
        let count = state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == path)
            .count();
        if state.deplete_after > 0 && count >= state.deplete_after {
            state.info.lock().unwrap()["life"]["energy_balance"] = json!(0);
        }
        let (status, body) = state
            .completions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| (500, json!({"error":"unexpected inference"}).to_string()));
        return Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
    }
    if path == "/env/v2/services" && state.block_service {
        state.service_started.notify_one();
        std::future::pending::<()>().await;
    }
    let value = match path.as_str() {
        "/env/v2/self" => state.info.lock().unwrap().clone(),
        "/env/v2/reports" => json!({"id":"report1"}),
        "/env/v2/models" => json!({"models":[{"id":"model-two"}]}),
        "/env/v2/name" => json!({"name":"Aster"}),
        "/env/v2/services" => json!({"registered":true}),
        "/env/v2/restart" => return StatusCode::NO_CONTENT.into_response(),
        path if path.starts_with("/env/v2/events") => json!({"events":[]}),
        "/env/v2/ledger" => json!({"reservations":[]}),
        _ => return (StatusCode::NOT_FOUND, "unexpected path").into_response(),
    };
    axum::Json(value).into_response()
}

fn completion(content: &str) -> Value {
    json!({"id":"mock-completion","choices":[{"message":{"role":"assistant","content":content},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
}
fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
}
fn calls(tools: Vec<Value>) -> Value {
    json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":tools},"finish_reason":"tool_calls"}]})
}
fn config(mock: &Mock, temp: &TempDir) -> Config {
    fs::write(temp.path().join("dna.md"), "creator prompt version one").unwrap();
    fs::create_dir(temp.path().join("source")).unwrap();
    fs::write(
        temp.path().join("boot.json"),
        r#"{"argv":["/opt/survive/survive-agent"]}"#,
    )
    .unwrap();
    let mut config = Config::new(&mock.url, TOKEN, temp.path().to_path_buf());
    config.http_timeout = Duration::from_secs(5);
    config.idle_delay = Duration::ZERO;
    config
}
fn turn_path(temp: &TempDir, turn: u64) -> std::path::PathBuf {
    temp.path().join(format!(".agent/history/{turn:020}"))
}
fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn reloads_prompt_before_every_inference_and_preserves_complete_history() {
    let first = calls(vec![call(
        "edit",
        "write_file",
        json!({"path":"dna.md","content":"creator prompt version two"}),
    )]);
    let mock = Mock::new(vec![first.clone(), completion("continue")]).await;
    let temp = TempDir::new().unwrap();
    let mut agent = Agent::open(config(&mock, &temp)).unwrap();
    assert_eq!(
        agent.step().await.unwrap(),
        StepOutcome::Advanced { turn: 1 }
    );
    agent.step().await.unwrap();
    let requests = mock.requests("/v1/chat/completions");
    assert_eq!(
        requests[0].body["messages"][1]["content"],
        "creator prompt version one"
    );
    assert_eq!(
        requests[1].body["messages"][1]["content"],
        "creator prompt version two"
    );
    assert_eq!(requests[1].body["model"], "mock-logical-model");
    assert_eq!(requests[1].body["stream"], false);
    assert_eq!(
        read_json(turn_path(&temp, 1).join("model/response.body")),
        first
    );
    assert_eq!(
        read_json(turn_path(&temp, 1).join("model-request.json")),
        requests[0].body
    );
    assert_eq!(
        fs::read_to_string(turn_path(&temp, 1).join("dna.md")).unwrap(),
        "creator prompt version one"
    );
    assert!(turn_path(&temp, 1).join("action-0000/intent.json").exists());
    assert!(turn_path(&temp, 1).join("action-0000/result.json").exists());
    assert!(
        requests[1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "edit")
    );
}

#[tokio::test]
async fn context_is_bounded_without_truncating_saved_responses() {
    let content = "large response 中文 ".repeat(4000);
    let mock = Mock::new((0..6).map(|_| completion(&content)).collect()).await;
    let temp = TempDir::new().unwrap();
    let mut config = config(&mock, &temp);
    config.max_request_bytes = 12 * 1024;
    let mut agent = Agent::open(config).unwrap();
    for _ in 0..6 {
        agent.step().await.unwrap();
    }
    for request in mock.requests("/v1/chat/completions") {
        assert!(serde_json::to_vec(&request.body).unwrap().len() <= 12 * 1024);
    }
    for id in 1..=6 {
        assert_eq!(
            read_json(turn_path(&temp, id).join("model/response.body"))["choices"][0]["message"]["content"],
            content
        );
    }
}

#[tokio::test]
async fn incomplete_or_malformed_batches_never_execute_any_tools() {
    let dangerous = call(
        "first",
        "write_file",
        json!({"path":"must-not-exist","content":"bad"}),
    );
    let mut truncated = calls(vec![dangerous.clone()]);
    truncated["choices"][0]["finish_reason"] = json!("length");
    let mut malformed = call("second", "shell", json!({"command":"true"}));
    malformed["function"]["arguments"] = json!("{\"command\":");
    let batch = calls(vec![dangerous, malformed]);
    let mock = Mock::configured(
        vec![
            (200, truncated.to_string()),
            (200, batch.to_string()),
            (200, "{\"choices\":[".into()),
        ],
        false,
        0,
    )
    .await;
    let temp = TempDir::new().unwrap();
    let mut agent = Agent::open(config(&mock, &temp)).unwrap();
    for _ in 0..3 {
        assert!(matches!(
            agent.step().await.unwrap(),
            StepOutcome::Backoff { .. }
        ));
    }
    assert!(!temp.path().join("must-not-exist").exists());
    assert_eq!(
        fs::read_to_string(turn_path(&temp, 3).join("model/response.body")).unwrap(),
        "{\"choices\":["
    );
    assert_eq!(mock.requests("/v1/chat/completions").len(), 3);
}

#[tokio::test]
async fn all_environment_and_file_tools_use_the_contract() {
    let response = calls(vec![
        call(
            "w",
            "write_file",
            json!({"path":"source/data","content":"abcdef"}),
        ),
        call(
            "r",
            "read_file",
            json!({"path":"source/data","offset":2,"max_bytes":3}),
        ),
        call(
            "sh",
            "shell",
            json!({"command":"printf 'hello'; printf 'error' >&2"}),
        ),
        call("self", "env", json!({"resource":"self"})),
        call(
            "events",
            "env",
            json!({"resource":"events","after":"a & b","limit":5}),
        ),
        call("ledger", "env", json!({"resource":"ledger"})),
        call(
            "report",
            "report",
            json!({"idempotency_key":"stable-key","body":"work","files":["source/data"]}),
        ),
        call("model", "choose_model", json!({"model":"model-two"})),
        call("name", "name", json!({"name":"Aster"})),
        call("service", "service", json!({"port":8080,"name":"demo"})),
        call("wait", "sleep", json!({"seconds":0})),
    ]);
    let mock = Mock::new(vec![response, completion("continue")]).await;
    let temp = TempDir::new().unwrap();
    let mut agent = Agent::open(config(&mock, &temp)).unwrap();
    agent.step().await.unwrap();
    let state = read_json(turn_path(&temp, 1).join("state.json"));
    assert!(
        state["actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["result"]["ok"] == true),
        "{state}"
    );
    assert_eq!(state["actions"][1]["result"]["text"], "cde");
    assert_eq!(state["actions"][1]["result"]["next_offset"], 5);
    assert_eq!(
        fs::read(turn_path(&temp, 1).join("action-0002/stdout")).unwrap(),
        b"hello"
    );
    assert_eq!(
        fs::read(turn_path(&temp, 1).join("action-0002/stderr")).unwrap(),
        b"error"
    );
    let submissions = mock.requests("/env/v2/reports");
    assert_eq!(submissions[0].method, "POST");
    assert_eq!(submissions[0].body["idempotency_key"], "stable-key");
    assert_eq!(
        mock.requests("/env/v2/events?after=a+%26+b&limit=5").len(),
        1
    );
    agent.step().await.unwrap();
    let request = &mock.requests("/v1/chat/completions")[1].body;
    assert_eq!(request["model"], "model-two");
    assert_eq!(mock.requests("/env/v2/name")[0].body["name"], "Aster");
    assert_eq!(
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .count(),
        11
    );
}

#[tokio::test]
async fn interrupted_side_effect_is_not_replayed_and_unstarted_actions_resume() {
    let mock = Mock::configured(
        vec![
            (
                200,
                calls(vec![
                    call(
                        "effect",
                        "service",
                        json!({"port":8123,"name":"recorded-before-disconnect"}),
                    ),
                    call(
                        "pending",
                        "write_file",
                        json!({"path":"pending-done","content":"once"}),
                    ),
                ])
                .to_string(),
            ),
            (200, completion("reconcile uncertain action").to_string()),
        ],
        true,
        0,
    )
    .await;
    let temp = TempDir::new().unwrap();
    let config = config(&mock, &temp);
    let mut agent = Agent::open(config.clone()).unwrap();
    {
        let step = agent.step();
        tokio::pin!(step);
        tokio::select! {
            result = &mut step => panic!("step should be awaiting side effect response: {result:?}"),
            _ = tokio::time::timeout(Duration::from_secs(3), mock.state.service_started.notified()) => {}
        }
    }
    assert_eq!(
        read_json(turn_path(&temp, 1).join("state.json"))["actions"][0]["status"],
        "running"
    );
    drop(agent);
    let mut recovered = Agent::open(config).unwrap();
    recovered.step().await.unwrap();
    assert_eq!(mock.requests("/env/v2/services").len(), 1);
    assert_eq!(
        fs::read_to_string(temp.path().join("pending-done")).unwrap(),
        "once"
    );
    let state = read_json(turn_path(&temp, 1).join("state.json"));
    assert_eq!(state["actions"][0]["status"], "uncertain");
    assert_eq!(state["actions"][1]["status"], "done");
    recovered.step().await.unwrap();
    let request = &mock.requests("/v1/chat/completions")[1].body;
    assert!(request["messages"].as_array().unwrap().iter().any(|m|m["role"] == "tool" && m["content"].as_str().unwrap().contains("NOT replayed")));
    assert_eq!(mock.requests("/env/v2/services").len(), 1);
}

#[tokio::test]
async fn restart_keeps_pending_batch_and_can_replace_the_entrypoint() {
    let temp = TempDir::new().unwrap();
    let executable = temp.path().join("replacement");
    let boot = json!({"argv":[executable]});
    let mock = Mock::new(vec![calls(vec![
        call("source","write_file",json!({"path":"source/replacement.rs","content":"fn main() { println!(\"replacement core\"); }"})),
        call("build","shell",json!({"command":"rustc source/replacement.rs -o replacement"})),
        call("boot","write_file",json!({"path":"boot.json","content":boot.to_string()})),
        call("restart","restart",json!({})),
        call("pending","write_file",json!({"path":"after-restart","content":"resumed"})),
    ])]).await;
    let config = config(&mock, &temp);
    let mut agent = Agent::open(config.clone()).unwrap();
    assert_eq!(agent.step().await.unwrap(), StepOutcome::RestartRequested);
    assert!(!temp.path().join("after-restart").exists());
    let build = read_json(turn_path(&temp, 1).join("action-0001/result.json"));
    assert_eq!(build["ok"], true, "{build}");
    assert_eq!(read_json(temp.path().join("boot.json")), boot);
    assert_eq!(
        std::process::Command::new(&executable)
            .output()
            .unwrap()
            .stdout,
        b"replacement core\n"
    );
    drop(agent);
    let mut recovered = Agent::open(config).unwrap();
    recovered.step().await.unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("after-restart")).unwrap(),
        "resumed"
    );
    assert_eq!(read_json(temp.path().join("boot.json")), boot);
    assert_eq!(mock.requests("/env/v2/restart").len(), 1);
    assert_eq!(mock.requests("/v1/chat/completions").len(), 1);
}

#[tokio::test]
async fn persisted_complete_model_response_is_recovered_without_another_inference() {
    let mock = Mock::new(vec![calls(vec![call(
        "write",
        "write_file",
        json!({"path":"recovered","content":"yes"}),
    )])])
    .await;
    let temp = TempDir::new().unwrap();
    let config = config(&mock, &temp);
    let mut agent = Agent::open(config.clone()).unwrap();
    agent.step().await.unwrap();
    drop(agent);
    // Recreate the precise crash boundary: response durable, Ready not committed.
    let state_path = turn_path(&temp, 1).join("state.json");
    let mut state = read_json(&state_path);
    state["stage"] = json!("request_started");
    state["assistant"] = Value::Null;
    state["actions"] = json!([]);
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    fs::remove_dir_all(turn_path(&temp, 1).join("action-0000")).unwrap();
    fs::remove_file(temp.path().join("recovered")).unwrap();
    let mut recovered = Agent::open(config).unwrap();
    recovered.step().await.unwrap();
    assert_eq!(mock.requests("/v1/chat/completions").len(), 1);
    assert_eq!(
        fs::read_to_string(temp.path().join("recovered")).unwrap(),
        "yes"
    );
}

#[tokio::test]
async fn an_uncertain_model_request_is_retained_and_never_replayed() {
    let mock = Mock::new(vec![completion("next new iteration")]).await;
    let temp = TempDir::new().unwrap();
    let config = config(&mock, &temp);
    fs::create_dir_all(turn_path(&temp, 1)).unwrap();
    fs::write(turn_path(&temp,1).join("state.json"),json!({"version":1,"id":1,"stage":"request_started","assistant":null,"actions":[],"note":null}).to_string()).unwrap();
    fs::write(
        turn_path(&temp, 1).join("model-request.json"),
        "{\"original\":true}",
    )
    .unwrap();
    let mut agent = Agent::open(config).unwrap();
    agent.step().await.unwrap();
    assert!(mock.requests("/v1/chat/completions").is_empty());
    assert!(
        read_json(turn_path(&temp, 1).join("state.json"))["note"]
            .as_str()
            .unwrap()
            .contains("uncertain")
    );
    agent.step().await.unwrap();
    assert_eq!(mock.requests("/v1/chat/completions").len(), 1);
    assert_eq!(
        read_json(turn_path(&temp, 1).join("model-request.json")),
        json!({"original":true})
    );
}

#[tokio::test]
async fn gateway_failures_back_off_without_human_input() {
    let mock = Mock::configured(
        vec![
            (503, json!({"error":"temporary"}).to_string()),
            (429, json!({"error":"busy"}).to_string()),
            (200, completion("done").to_string()),
        ],
        false,
        3,
    )
    .await;
    let temp = TempDir::new().unwrap();
    let mut config = config(&mock, &temp);
    config.backoff_initial = Duration::from_millis(40);
    config.backoff_max = Duration::from_millis(80);
    let mut agent = Agent::open(config).unwrap();
    let running = tokio::spawn(async move { agent.run().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while mock.requests("/env/v2/self").len() < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!running.is_finished());
    running.abort();
    let requests = mock.requests("/v1/chat/completions");
    assert_eq!(requests.len(), 3);
    assert!(requests[1].at.duration_since(requests[0].at) >= Duration::from_millis(40));
    assert!(requests[2].at.duration_since(requests[1].at) >= Duration::from_millis(80));
    assert_eq!(
        read_json(turn_path(&temp, 1).join("model/response.body")),
        json!({"error":"temporary"})
    );
}

#[tokio::test]
async fn balances_locking_and_oversized_prompt_fail_safely() {
    let mock = Mock::new(vec![]).await;
    let temp = TempDir::new().unwrap();
    let config = config(&mock, &temp);
    let mut agent = Agent::open(config.clone()).unwrap();
    assert!(Agent::open(config).is_err());
    mock.state.info.lock().unwrap()["life"]["energy_available"] = json!(0);
    assert_eq!(agent.step().await.unwrap(), StepOutcome::WaitingForBalance);
    mock.state.info.lock().unwrap()["life"]["energy_available"] = json!(10000);
    fs::write(temp.path().join("dna.md"), "p".repeat(100000)).unwrap();
    assert!(agent.step().await.unwrap_err().to_string().contains("DNA"));
    assert!(mock.requests("/v1/chat/completions").is_empty());
    assert_eq!(
        fs::metadata(turn_path(&temp, 2).join("dna.md"))
            .unwrap()
            .len(),
        100000
    );
}

#[tokio::test]
async fn shell_timeout_preserves_output_and_records_possible_partial_effects() {
    let mock = Mock::new(vec![calls(vec![call(
        "shell",
        "shell",
        json!({"command":"printf 'before timeout'; while :; do :; done"}),
    )])])
    .await;
    let temp = TempDir::new().unwrap();
    let mut config = config(&mock, &temp);
    config.shell_timeout = Duration::from_millis(50);
    let mut agent = Agent::open(config).unwrap();
    agent.step().await.unwrap();
    let result = read_json(turn_path(&temp, 1).join("action-0000/result.json"));
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["outcome"], "partial_effects_possible");
    assert_eq!(
        fs::read(turn_path(&temp, 1).join("action-0000/stdout")).unwrap(),
        b"before timeout"
    );
}

#[tokio::test]
async fn shell_effect_is_not_repeated_with_or_without_a_durable_result() {
    for result_is_durable in [false, true] {
        let mock = Mock::new(vec![calls(vec![call(
            "append",
            "shell",
            json!({"command":"printf 'once' >> effect; printf 'full output'"}),
        )])])
        .await;
        let temp = TempDir::new().unwrap();
        let config = config(&mock, &temp);
        let mut agent = Agent::open(config.clone()).unwrap();
        agent.step().await.unwrap();
        drop(agent);
        // Simulate a crash after the external effect, on either side of the
        // durable result write and before the final checkpoint commit.
        let state_path = turn_path(&temp, 1).join("state.json");
        let mut state = read_json(&state_path);
        state["stage"] = json!("ready");
        state["actions"][0]["status"] = json!("running");
        state["actions"][0]["result"] = Value::Null;
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        if !result_is_durable {
            fs::remove_file(turn_path(&temp, 1).join("action-0000/result.json")).unwrap();
        }
        let mut recovered = Agent::open(config).unwrap();
        recovered.step().await.unwrap();
        assert_eq!(fs::read(temp.path().join("effect")).unwrap(), b"once");
        assert_eq!(
            fs::read(turn_path(&temp, 1).join("action-0000/stdout")).unwrap(),
            b"full output"
        );
        assert_eq!(
            read_json(&state_path)["actions"][0]["status"],
            if result_is_durable {
                "done"
            } else {
                "uncertain"
            }
        );
        assert!(turn_path(&temp, 1).join("action-0000/result.json").exists());
        assert_eq!(mock.requests("/v1/chat/completions").len(), 1);
    }
}

#[tokio::test]
async fn oversized_response_is_fully_saved_but_never_executed() {
    let response = calls(vec![call(
        "large",
        "write_file",
        json!({"path":"should-not-exist","content":"x".repeat(8000)}),
    )]);
    let mock = Mock::new(vec![response.clone()]).await;
    let temp = TempDir::new().unwrap();
    let mut config = config(&mock, &temp);
    config.max_response_bytes = 1024;
    let mut agent = Agent::open(config).unwrap();
    assert!(matches!(
        agent.step().await.unwrap(),
        StepOutcome::Backoff { .. }
    ));
    assert_eq!(
        read_json(turn_path(&temp, 1).join("model/response.body")),
        response
    );
    assert!(!temp.path().join("should-not-exist").exists());
}

#[tokio::test]
async fn gpt6_seed_uses_public_id_and_executes_only_complete_responses_tools() {
    let mut answer = json!({"id":"resp_test","object":"response","status":"incomplete","output":[{"type":"function_call","status":"completed","call_id":"call_test","name":"write_file","arguments":r#"{"path":"proof","content":"ok"}"#}]});
    let incomplete = answer.clone();
    answer["status"] = json!("completed");
    let mock = Mock::new(vec![
        incomplete,
        answer,
        json!({"id":"resp_next","object":"response","status":"completed","output":[]}),
    ])
    .await;
    let temp = TempDir::new().unwrap();
    mock.state.info.lock().unwrap()["organism"]["default_model"] = json!("gpt-6.2-sol");
    let mut agent = Agent::open(config(&mock, &temp)).unwrap();
    assert!(matches!(
        agent.step().await.unwrap(),
        StepOutcome::Backoff { .. }
    ));
    assert!(!temp.path().join("proof").exists());
    agent.step().await.unwrap();
    agent.step().await.unwrap();
    assert_eq!(fs::read_to_string(temp.path().join("proof")).unwrap(), "ok");
    let req = mock.requests("/v1/responses");
    assert_eq!(req[0].body["model"], "gpt-6.2-sol");
    assert!(
        req[0].body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "report" && t["type"] == "function")
    );
    assert!(
        req[2].body["input"]
            .to_string()
            .contains("Recorded tool result")
    );
    assert!(mock.requests("/v1/chat/completions").is_empty());
}

#[tokio::test]
async fn costly_model_reserves_a_response_the_current_balance_can_cover() {
    let response = json!({"id":"priced","object":"response","status":"completed","output":[]});
    let mock = Mock::new(vec![response]).await;
    let temp = TempDir::new().unwrap();
    let mut agent = Agent::open(config(&mock, &temp)).unwrap();
    {
        let mut info = mock.state.info.lock().unwrap();
        info["organism"]["default_model"] = json!("gpt-6.2-astra");
        info["models"] = json!([{"id":"gpt-6.2-astra","input":80,"output":400}]);
        info["minimum_input_reservation"] = json!(1024);
        info["life"]["energy_balance"] = json!(1_000_000);
        info["life"]["energy_available"] = json!(1_000_000);
    }
    agent.step().await.unwrap();
    let request = &mock.requests("/v1/responses")[0].body;
    let output = request["max_output_tokens"].as_u64().unwrap();
    assert!((16..4096).contains(&output));
    assert!(serde_json::to_vec(request).unwrap().len() as u64 * 80 + output * 400 <= 1_000_000);
    mock.state.info.lock().unwrap()["life"]["energy_available"] = json!(1);
    assert_eq!(agent.step().await.unwrap(), StepOutcome::WaitingForBalance);
    assert_eq!(mock.requests("/v1/responses").len(), 1);
}
